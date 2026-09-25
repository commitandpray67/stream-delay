//! The stream delay engine.
//!
//! Every message from the encoder is appended to a ring buffer together with the
//! time it arrived. The output reads from a cursor into that buffer and sends a
//! message once `now >= arrival + delay`. Changing the delay moves the cursor to a
//! keyframe (a *splice*) and shifts timestamps so the output keeps counting up. See
//! `docs/adr/0002-keyframe-splicing.md`.
//!
//! The engine is sans-IO: callers pass the current time (microseconds on any
//! monotonic clock) into every call, which keeps it fully deterministic in tests.

mod snapshot;

use std::collections::VecDeque;

use bytes::Bytes;
use serde::{Deserialize, Serialize};
use thiserror::Error;

pub use snapshot::{IngestStats, OutputStats, Phase, Snapshot};
use streamdelay_flv as flv;

/// Microseconds on a monotonic clock chosen by the caller.
pub type Time = u64;

const MS: u64 = 1_000;
const SEC: u64 = 1_000_000;

/// Decoder configuration messages kept per encoder session, to resend after a
/// splice. Real streams have one or two per track; the limits only keep a
/// misbehaving encoder from holding much. What is kept counts against the
/// memory cap (see [`Session::cost`]), and takes at most
/// [`HEADER_SHARE_OF_RAM_CAP`] of it: eviction cannot drop a live session's.
const MAX_HEADERS: usize = 64;
const MAX_HEADER_BYTES: usize = 1024 * 1024;
/// The retained configuration of a session takes at most the RAM cap divided by
/// this (2 MiB at the smallest cap, 16 MiB).
const HEADER_SHARE_OF_RAM_CAP: usize = 8;
/// Largest stream metadata kept per session: an encoder's is about a kilobyte.
const MAX_METADATA_BYTES: usize = 64 * 1024;

/// Tunables for the engine.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct EngineConfig {
    /// Largest delay that can be requested.
    pub max_delay_ms: u64,
    /// History kept beyond `max_delay_ms`, so rewinds can round to a keyframe.
    pub headroom_ms: u64,
    /// Hard cap on buffered bytes; the oldest content is dropped beyond it.
    pub ram_cap_bytes: usize,
    /// In mask mode, how long after the slate appears the rewind point may start.
    pub mask_margin_ms: u64,
    /// Keep a rolling history (up to `max_delay_ms`) so delay can be added by
    /// rewinding. When false, content is dropped once it has aired and every delay
    /// increase uses mask mode, which builds the delay from new content.
    pub keep_history: bool,
}

impl Default for EngineConfig {
    fn default() -> Self {
        Self {
            max_delay_ms: 120_000,
            headroom_ms: 10_000,
            ram_cap_bytes: 512 * 1024 * 1024,
            mask_margin_ms: 500,
            keep_history: true,
        }
    }
}

/// Kind of message in the stream.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Kind {
    Audio,
    Video,
    /// AMF0 data such as metadata, captions and cue points.
    Data,
}

impl Kind {
    fn index(self) -> usize {
        match self {
            Kind::Audio => 0,
            Kind::Video => 1,
            Kind::Data => 2,
        }
    }
}

/// A message ready to be sent to the destination.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct OutMsg {
    pub kind: Kind,
    /// Output timestamp in milliseconds (already rewritten and wrapped to 32 bits).
    pub timestamp: u32,
    pub payload: Bytes,
    /// Buffer sequence number, or `None` for re-sent metadata and decoder configuration.
    pub seq: Option<u64>,
}

/// How to add delay.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum DelayMode {
    /// Jump back immediately; viewers see the last few seconds again.
    #[default]
    Rewind,
    /// Show the overlay slate, then rewind only to content recorded under the slate.
    Mask,
}

/// When to drop the delay.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum GoLiveWhen {
    /// At the next keyframe from the encoder; buffered content is skipped.
    Now,
    /// After everything buffered up to now has been sent.
    AfterAir,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum Command {
    SetDelay {
        ms: u64,
        mode: DelayMode,
    },
    GoLive(GoLiveWhen),
    /// Cancels a pending change.
    Cancel,
    /// Throws away everything that has not aired yet, keeping the broadcast going
    /// with the same delay. Rewind: viewers see the last stretch of the stream
    /// again (as when adding delay), then it continues with what was recorded
    /// after the dump. Mask, or not enough history for that: the overlay slate
    /// covers the stream while the delay builds back up.
    Dump(DelayMode),
}

/// Outcome of a command.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct Ack {
    pub target_ms: u64,
    pub effective_ms: u64,
    /// The change will complete later (mask fill, waiting for a keyframe, ...).
    pub pending: bool,
    /// Less history was available than requested; the delay is shorter than asked.
    pub history_short: bool,
}

#[derive(Debug, Clone, Error, PartialEq, Eq)]
pub enum EngineError {
    #[error("requested delay of {requested_ms} ms exceeds the maximum of {max_ms} ms")]
    TooLarge { requested_ms: u64, max_ms: u64 },
    #[error("there is no delay, so nothing is waiting to air")]
    NothingToDump,
}

/// Memory a buffered message takes besides its payload: its [`Entry`], its slot
/// in the ring and the payload's handle. Counting it keeps the RAM cap meaningful
/// for streams of many tiny messages, which would otherwise use many times the
/// cap before any were dropped.
const ENTRY_OVERHEAD: usize = std::mem::size_of::<Entry>() + 64;

/// What a buffered message with a payload of `len` bytes counts against the RAM cap.
fn cost(len: usize) -> usize {
    len + ENTRY_OVERHEAD
}

struct Entry {
    seq: u64,
    arrival: Time,
    session: u32,
    kind: Kind,
    ts: u64,
    payload: Bytes,
    /// A point the output can start from (video keyframe, or any audio frame in an
    /// audio-only stream).
    sync: bool,
    /// Decoder configuration class, for configuration messages.
    config: Option<u16>,
    /// Composition time offset (PTS - DTS) of video frames, in ms.
    cts: i32,
    /// HEVC NAL unit type of the first slice, if known.
    hevc_nal: Option<u8>,
    /// Offset of HEVC NAL unit data in the payload.
    nal_offset: Option<usize>,
}

#[derive(Clone)]
struct HeaderRec {
    seq: u64,
    kind: Kind,
    class: u16,
    payload: Bytes,
}

struct Session {
    id: u32,
    headers: Vec<HeaderRec>,
    metadata: Option<Bytes>,
    has_video: bool,
    last_video_ts: Option<u64>,
    frame_ms: u64,
}

impl Session {
    /// What it counts against the RAM cap, like buffered messages.
    fn cost(&self) -> usize {
        let headers: usize = self.headers.iter().map(|h| cost(h.payload.len())).sum();
        cost(self.metadata.as_ref().map_or(0, Bytes::len)) + headers
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Pending {
    None,
    /// Splice to a keyframe that arrives after `after_seq`.
    GoLiveNow {
        after_seq: u64,
    },
    /// Wait until `mark_seq` has been sent, then go live.
    AfterAir {
        mark_seq: u64,
    },
    /// Skip forward to the newest keyframe at least `delay` old.
    Reduce {
        delay: u64,
    },
    Mask {
        delay: u64,
        started: Time,
        anchor: Option<u64>,
    },
    /// Everything recorded before `started` was thrown away; the output waits for
    /// a keyframe recorded under the slate, then builds `delay` back up behind it.
    Dump {
        delay: u64,
        started: Time,
    },
    /// What had not aired was thrown away and the output replays what aired
    /// before it. Nothing from `from` on airs until the first keyframe there is
    /// `delay` old; the output continues from it.
    Replay {
        from: u64,
        delay: u64,
    },
}

impl Pending {
    /// The overlay slate is up for this change.
    fn covers(&self) -> bool {
        matches!(self, Pending::Mask { .. } | Pending::Dump { .. })
    }
}

struct Output {
    connected: bool,
    /// Bumped on every connect so stale messages can be told apart.
    generation: u64,
    next_seq: u64,
    delay: u64,
    target: u64,
    ts_offset: i64,
    fresh: bool,
    need_sync: bool,
    started: bool,
    current_session: Option<u32>,
    last_out_ts: u64,
    last_kind_ts: [Option<u64>; 3],
    /// Highest presentation time sent (video DTS + composition time).
    last_pts: i64,
    /// After splicing to an HEVC CRA frame (open GOP), its RASL leading pictures
    /// reference frames that were never sent and must be dropped.
    skip_rasl_after: Option<u64>,
    gate: Option<(u32, u64)>,
    pending_headers: bool,
    sent_headers: Vec<(Kind, u16, Bytes)>,
    sent_metadata: Option<Bytes>,
    pending: Pending,
    mask_visible: bool,
    history_short: bool,
    /// Nothing after this sequence number is sent (see [`Engine::end_after`]).
    end_mark: Option<u64>,
    splices: u64,
    dropped: u64,
    sent_bytes: u64,
    /// First sequence number emitted on the current connection.
    first_emitted: Option<u64>,
}

/// The delay engine. See the crate docs.
pub struct Engine {
    config: EngineConfig,
    ring: VecDeque<Entry>,
    syncs: VecDeque<u64>,
    base_seq: u64,
    next_seq: u64,
    bytes: usize,
    /// Encoder sessions whose messages are buffered, and the current one. Their
    /// headers and metadata take `session_bytes` (see [`Session::cost`]).
    sessions: Vec<Session>,
    session_bytes: usize,
    ingest_active: bool,
    next_session: u32,
    out: Output,
    stats: snapshot::IngestTracker,
}

impl Engine {
    pub fn new(config: EngineConfig) -> Self {
        Self {
            config,
            ring: VecDeque::new(),
            syncs: VecDeque::new(),
            base_seq: 0,
            next_seq: 0,
            bytes: 0,
            sessions: Vec::new(),
            session_bytes: 0,
            ingest_active: false,
            next_session: 1,
            out: Output {
                connected: false,
                generation: 0,
                next_seq: 0,
                delay: 0,
                target: 0,
                ts_offset: 0,
                fresh: true,
                need_sync: true,
                started: false,
                current_session: None,
                last_out_ts: 0,
                last_kind_ts: [None; 3],
                last_pts: 0,
                skip_rasl_after: None,
                gate: None,
                pending_headers: false,
                sent_headers: Vec::new(),
                sent_metadata: None,
                pending: Pending::None,
                mask_visible: false,
                history_short: false,
                end_mark: None,
                splices: 0,
                dropped: 0,
                sent_bytes: 0,
                first_emitted: None,
            },
            stats: snapshot::IngestTracker::default(),
        }
    }

    pub fn config(&self) -> &EngineConfig {
        &self.config
    }

    /// Turns the rolling history on or off (see [`EngineConfig::keep_history`]).
    /// Turning it off drops aired content on the next eviction.
    pub fn set_keep_history(&mut self, keep: bool) {
        self.config.keep_history = keep;
    }

    // ----- ingest ---------------------------------------------------------------

    /// A publisher connected. Returns the new session id.
    pub fn ingest_start(&mut self, _now: Time) -> u32 {
        // Unless a broadcast is still running (an encoder reconnecting within the
        // grace period), what is left of an earlier stream must not be rewound into.
        if !self.out.started {
            self.drop_buffer();
        }
        // The last session gets no more messages: if none is buffered, nothing
        // needs it (an encoder that reconnects over and over, sending no media,
        // must not pile them up).
        if let Some(last) = self.sessions.last()
            && self.ring.back().is_none_or(|e| e.session != last.id)
        {
            self.sessions.pop();
        }
        let id = self.next_session;
        self.next_session += 1;
        self.sessions.push(Session {
            id,
            headers: Vec::new(),
            metadata: None,
            has_video: false,
            last_video_ts: None,
            frame_ms: 33,
        });
        self.count_sessions();
        self.ingest_active = true;
        self.stats = snapshot::IngestTracker::default();
        id
    }

    /// The publisher disconnected. Buffered content keeps draining.
    pub fn ingest_end(&mut self, _now: Time) {
        self.ingest_active = false;
    }

    pub fn ingest_active(&self) -> bool {
        self.ingest_active
    }

    /// Stream metadata (`@setDataFrame onMetaData ...`). Ignored if larger than
    /// an encoder's would be.
    pub fn ingest_metadata(&mut self, _now: Time, payload: Bytes) {
        if payload.len() > MAX_METADATA_BYTES {
            return;
        }
        if let Some(s) = self.sessions.last_mut() {
            // A copy: the original shares its allocation with neighbouring
            // messages, which would otherwise stay in memory as long as this.
            s.metadata = Some(Bytes::copy_from_slice(&payload));
            self.count_sessions();
        }
    }

    /// Updates `session_bytes` after the sessions changed.
    fn count_sessions(&mut self) {
        self.session_bytes = self.sessions.iter().map(Session::cost).sum();
    }

    /// Appends one message from the publisher. `ts` is the unwrapped timestamp in ms.
    pub fn ingest(&mut self, now: Time, kind: Kind, ts: u64, payload: Bytes) {
        if !self.ingest_active || payload.is_empty() {
            return;
        }
        let Some(session) = self.sessions.last_mut() else {
            return;
        };
        let (mut sync, mut config, mut cts, mut hevc_nal, mut nal_offset) =
            (false, None, 0, None, None);
        match kind {
            Kind::Video => {
                if let Some(info) = flv::inspect_video(&payload) {
                    self.stats.video(now, &info, ts);
                    session.has_video = true;
                    sync = info.keyframe;
                    cts = info.composition_time;
                    hevc_nal = info.hevc_nal_type;
                    nal_offset = info.nal_offset;
                    if info.config {
                        config = Some(info.config_class);
                    } else if let Some(last) = session.last_video_ts {
                        let d = ts.saturating_sub(last);
                        if (1..=200).contains(&d) {
                            session.frame_ms = d;
                        }
                    }
                    if !info.config {
                        session.last_video_ts = Some(ts);
                    }
                }
            }
            Kind::Audio => {
                if let Some(info) = flv::inspect_audio(&payload) {
                    self.stats.audio(&info);
                    if info.config {
                        config = Some(info.config_class);
                    } else {
                        sync = !session.has_video;
                    }
                }
            }
            Kind::Data => {}
        }
        let seq = self.next_seq;
        self.next_seq += 1;
        let budget = self.config.ram_cap_bytes / HEADER_SHARE_OF_RAM_CAP;
        if let Some(class) = config
            && payload.len() <= MAX_HEADER_BYTES
            && cost(payload.len()) <= budget
        {
            session
                .headers
                .retain(|h| !(h.kind == kind && h.class == class));
            // The oldest go, to keep within the count and the budget.
            let mut kept: usize = session.headers.iter().map(|h| cost(h.payload.len())).sum();
            while session.headers.len() >= MAX_HEADERS || kept + cost(payload.len()) > budget {
                kept -= cost(session.headers.remove(0).payload.len());
            }
            session.headers.push(HeaderRec {
                seq,
                kind,
                class,
                // A copy: the original shares its allocation with neighbouring
                // messages, which would otherwise stay in memory as long as this.
                payload: Bytes::copy_from_slice(&payload),
            });
        }
        if sync {
            self.syncs.push_back(seq);
        }
        self.stats.bytes(now, payload.len());
        self.bytes += cost(payload.len());
        let session = session.id;
        if config.is_some() {
            self.count_sessions();
        }
        self.ring.push_back(Entry {
            seq,
            arrival: now,
            session,
            kind,
            ts,
            payload,
            sync,
            config,
            cts,
            hevc_nal,
            nal_offset,
        });
        self.evict(now);
    }

    // ----- output lifecycle -----------------------------------------------------

    /// The destination connection is ready. Returns the generation to tag messages with.
    pub fn output_connected(&mut self, now: Time) -> u64 {
        let o = &mut self.out;
        o.connected = true;
        o.generation += 1;
        o.fresh = true;
        o.last_out_ts = 0;
        o.last_kind_ts = [None; 3];
        o.last_pts = 0;
        o.sent_headers.clear();
        o.sent_metadata = None;
        o.first_emitted = None;
        // Restart from the keyframe at or before the cursor so the new connection
        // starts with a decodable frame.
        if self.out.started {
            let cursor = self.out.next_seq;
            match self.syncs.iter().rev().find(|&&s| s <= cursor).copied() {
                Some(k) => {
                    let arrival = self.entry(k).map_or(now, |e| e.arrival);
                    let delay = self.out.delay.max(now.saturating_sub(arrival));
                    self.splice_to(k, delay);
                }
                None => self.out.need_sync = true,
            }
        }
        self.out.generation
    }

    /// The destination connection dropped. `last_written` is the last sequence number
    /// that was fully written, so resending can resume right after it.
    pub fn output_disconnected(&mut self, _now: Time, last_written: Option<u64>) {
        self.out.connected = false;
        let resume = match last_written {
            Some(s) => Some(s + 1),
            None => self.out.first_emitted,
        };
        if let Some(r) = resume {
            self.out.next_seq = self.out.next_seq.min(r);
        }
    }

    /// Output after `taken` (the last message the destination connection took;
    /// `None`: none yet) was emitted but is still queued, and will never be sent:
    /// it counts as not aired, so it is sent again, or thrown away by a dump.
    /// Codec headers are sent again, in case some were among it.
    pub fn unsend(&mut self, taken: Option<u64>) {
        if !self.out.connected {
            return;
        }
        let resume = match taken {
            Some(s) => Some(s + 1),
            None => self.out.first_emitted,
        };
        if let Some(r) = resume {
            self.out.next_seq = self.out.next_seq.min(r);
        }
        self.out.sent_headers.clear();
        self.out.sent_metadata = None;
    }

    /// Ends the current broadcast on the output side. The next encoder session starts
    /// a fresh broadcast (with the current target delay) instead of continuing this one.
    pub fn output_reset(&mut self) {
        let o = &mut self.out;
        o.connected = false;
        o.started = false;
        o.need_sync = true;
        o.current_session = None;
        o.next_seq = self.next_seq;
        o.delay = o.target;
        o.history_short = false;
        o.gate = None;
        o.end_mark = None;
        if o.pending.covers() {
            o.mask_visible = false;
        }
        o.pending = Pending::None;
    }

    /// Drops everything buffered and ends the current broadcast on the output side
    /// (see [`Engine::output_reset`]). Nothing received before this call is ever
    /// sent; the output starts again from the next keyframe, with the current
    /// target delay. Used for "end stream" and after a broadcast has finished.
    pub fn discard(&mut self) {
        self.drop_buffer();
        self.output_reset();
    }

    fn drop_buffer(&mut self) {
        self.ring.clear();
        self.syncs.clear();
        self.bytes = 0;
        self.base_seq = self.next_seq;
        // Keep the running encoder session: its codec headers are needed to start
        // the next broadcast.
        let current = self
            .ingest_active
            .then(|| self.sessions.last().map(|s| s.id))
            .flatten();
        self.sessions.retain(|s| Some(s.id) == current);
        self.count_sessions();
        self.out.next_seq = self.next_seq;
        self.out.need_sync = true;
    }

    pub fn output_generation(&self) -> u64 {
        self.out.generation
    }

    /// True when there is (or soon will be) something to send, so the destination
    /// should be connected.
    pub fn output_wanted(&self, now: Time) -> bool {
        if self.out.connected && !self.drained() {
            return true;
        }
        let lead = 3 * SEC;
        let cursor = if self.out.need_sync {
            self.syncs
                .iter()
                .find(|&&s| s >= self.out.next_seq)
                .copied()
        } else {
            Some(self.out.next_seq)
        };
        cursor
            .and_then(|s| self.entry(s))
            .is_some_and(|e| e.arrival + self.out.delay <= now + lead)
    }

    /// True when everything in the buffer has been sent.
    pub fn drained(&self) -> bool {
        self.out.next_seq >= self.next_seq
    }

    /// True while the destination connection is up (between
    /// [`Engine::output_connected`] and [`Engine::output_disconnected`]).
    pub fn output_is_connected(&self) -> bool {
        self.out.connected
    }

    /// The delay viewers get (or will get, before the broadcast starts).
    pub fn effective_delay(&self) -> Time {
        if self.out.started {
            self.out.delay
        } else {
            self.out.target
        }
    }

    /// True when anything is buffered.
    pub fn has_buffered(&self) -> bool {
        !self.ring.is_empty()
    }

    /// The newest sequence number received, if any.
    pub fn last_seq(&self) -> Option<u64> {
        self.next_seq.checked_sub(1)
    }

    /// Sends what arrived up to `seq` (inclusive), and nothing after it: the end
    /// of a broadcast. [`Engine::end_reached`] tells when it has all been sent.
    pub fn end_after(&mut self, seq: u64) {
        self.out.end_mark = Some(seq);
    }

    /// Forgets the mark set with [`Engine::end_after`]: the output carries on.
    pub fn cancel_end(&mut self) {
        self.out.end_mark = None;
    }

    /// True once everything up to the mark set with [`Engine::end_after`] has been
    /// handed over (or skipped by a delay change).
    pub fn end_reached(&self) -> bool {
        self.out.end_mark.is_some_and(|m| self.out.next_seq > m)
    }

    /// Ends the broadcast at the end mark and prepares a new one for what arrived
    /// after it: the output starts again, with the target delay, from the first
    /// keyframe after the mark. What came before the mark is dropped, so the new
    /// broadcast cannot rewind into the old one.
    pub fn restart_after_end(&mut self) {
        let from = self.out.end_mark.map_or(self.next_seq, |m| m + 1);
        self.drop_before(from);
        self.output_reset();
        self.out.next_seq = from.max(self.base_seq);
    }

    /// Drops everything before `seq`.
    fn drop_before(&mut self, seq: u64) {
        while self.ring.front().is_some_and(|f| f.seq < seq) {
            let f = self.ring.pop_front().expect("front exists");
            self.bytes -= cost(f.payload.len());
            self.base_seq = f.seq + 1;
            if self.syncs.front() == Some(&f.seq) {
                self.syncs.pop_front();
            }
        }
        if self.ring.is_empty() {
            self.base_seq = self.base_seq.max(seq.min(self.next_seq));
        }
    }

    // ----- commands -------------------------------------------------------------

    pub fn command(&mut self, now: Time, cmd: Command) -> Result<Ack, EngineError> {
        match cmd {
            // A dump's replay is not a change to cancel: its delay is the target.
            Command::Cancel if matches!(self.out.pending, Pending::Replay { .. }) => {}
            Command::Cancel => {
                self.set_pending(Pending::None);
                self.out.target = self.out.delay;
            }
            Command::Dump(_) if !self.can_dump() => return Err(EngineError::NothingToDump),
            Command::Dump(mode) => {
                if !self.out.started {
                    // Nothing has aired yet: throwing the buffer away is enough.
                    self.drop_buffer();
                    return Ok(self.ack());
                }
                if self.out.end_mark.is_some() || !self.ingest_active {
                    // The broadcast is ending (or the encoder has stopped, so
                    // nothing follows): it ends now, without the rest.
                    self.drop_buffer();
                    self.set_pending(Pending::None);
                    return Ok(self.ack());
                }
                let delay = self.dump_delay();
                self.out.history_short = false;
                self.out.target = delay;
                if mode == DelayMode::Rewind && self.replay_instead(now, delay) {
                    return Ok(self.ack());
                }
                // Gone for good, including what a rewind could reach.
                self.drop_buffer();
                self.set_pending(Pending::Dump {
                    delay,
                    started: now,
                });
                self.out.mask_visible = true;
            }
            Command::GoLive(when) => {
                self.out.target = 0;
                self.out.history_short = false;
                let last = self.next_seq.saturating_sub(1);
                if !self.out.started {
                    self.out.delay = 0;
                    self.set_pending(Pending::None);
                } else {
                    self.set_pending(match when {
                        GoLiveWhen::Now => Pending::GoLiveNow { after_seq: last },
                        GoLiveWhen::AfterAir => Pending::AfterAir { mark_seq: last },
                    });
                }
            }
            Command::SetDelay { ms, mode } => {
                if ms > self.config.max_delay_ms {
                    return Err(EngineError::TooLarge {
                        requested_ms: ms,
                        max_ms: self.config.max_delay_ms,
                    });
                }
                let d = ms * MS;
                self.out.target = d;
                self.out.history_short = false;
                self.set_pending(Pending::None);
                if !self.out.started {
                    // Nothing sent yet: the stream simply starts with this delay.
                    self.out.delay = d;
                } else if ms == 0 {
                    return self.command(now, Command::GoLive(GoLiveWhen::Now));
                } else if d > self.out.delay {
                    // Without history there is nothing to rewind into.
                    let mode = if self.config.keep_history {
                        mode
                    } else {
                        DelayMode::Mask
                    };
                    match mode {
                        DelayMode::Rewind => self.rewind(now, d),
                        DelayMode::Mask => {
                            self.set_pending(Pending::Mask {
                                delay: d,
                                started: now,
                                anchor: None,
                            });
                            self.out.mask_visible = true;
                        }
                    }
                } else if d < self.out.delay {
                    self.set_pending(Pending::Reduce { delay: d });
                }
            }
        }
        self.run_pending(now);
        Ok(self.ack())
    }

    /// The delay a dump goes back to: the one asked for, or while it is being
    /// removed, the protection that was in effect.
    fn dump_delay(&self) -> u64 {
        if self.out.target > 0 {
            self.out.target
        } else {
            self.out.delay
        }
    }

    /// False when a dump would be refused: live (as the snapshot counts it), what
    /// is in flight airs before anyone could react.
    pub fn can_dump(&self) -> bool {
        !self.out.started
            || self.out.end_mark.is_some()
            || !self.ingest_active
            || self.dump_delay() >= 500 * MS
    }

    fn ack(&self) -> Ack {
        let effective = match self.out.pending {
            Pending::Replay { delay, .. } => delay,
            _ => self.out.delay,
        };
        Ack {
            target_ms: self.out.target / MS,
            effective_ms: effective / MS,
            pending: self.out.pending != Pending::None,
            history_short: self.out.history_short,
        }
    }

    fn set_pending(&mut self, p: Pending) {
        if self.out.pending.covers() {
            self.out.mask_visible = false;
        }
        self.out.pending = p;
    }

    /// Jumps back to the newest keyframe at least `d` old (or the oldest available).
    fn rewind(&mut self, now: Time, d: u64) {
        let newest = self.newest_sync_arrived_by(now.checked_sub(d));
        let k = match newest {
            Some(k) => Some(k),
            None => {
                self.out.history_short = true;
                self.syncs.front().copied()
            }
        };
        if let Some(k) = k {
            let arrival = self.entry(k).map(|e| e.arrival).unwrap_or(now);
            let delay = now.saturating_sub(arrival);
            if delay > self.out.delay {
                self.splice_to(k, delay);
            }
        }
    }

    /// For a dump: throws away what has not aired and replays the stretch that
    /// aired before it, which lasts until what is recorded from now on is `delay`
    /// old. Returns false, changing nothing, if the buffer does not reach back
    /// far enough.
    fn replay_instead(&mut self, now: Time, delay: u64) -> bool {
        if !self.config.keep_history {
            return false;
        }
        let cut = self.out.next_seq.max(self.base_seq);
        // When the first thing not aired yet was recorded.
        let edge = self.entry(cut).map_or(now, |e| e.arrival);
        // At least `delay` before it, so the replay keeps the delay too.
        let Some(k) = self.newest_sync_arrived_by(edge.checked_sub(delay)) else {
            return false;
        };
        // Cut the rest off the buffer. What is recorded next reuses its sequence
        // numbers: nothing refers to them, since none of it was ever sent.
        let keep = (cut - self.base_seq) as usize;
        for e in self.ring.drain(keep..) {
            self.bytes -= cost(e.payload.len());
        }
        self.syncs.retain(|&s| s < cut);
        self.next_seq = cut;
        self.split_session(cut);
        self.set_pending(Pending::Replay { from: cut, delay });
        let arrival = self.entry(k).map_or(now, |e| e.arrival);
        self.splice_to(k, now.saturating_sub(arrival));
        true
    }

    /// Records what arrives from `seq` on as a new session of the same encoder,
    /// so the output restarts there at a keyframe, as after a reconnect: what
    /// came right before it is gone.
    fn split_session(&mut self, seq: u64) {
        let Some(current) = self.sessions.last() else {
            return;
        };
        let mut next = Session {
            id: self.next_session,
            headers: current.headers.clone(),
            metadata: current.metadata.clone(),
            has_video: current.has_video,
            last_video_ts: current.last_video_ts,
            frame_ms: current.frame_ms,
        };
        // Decoder configuration recorded in the cut part is still needed.
        for h in &mut next.headers {
            h.seq = h.seq.min(seq.saturating_sub(1));
        }
        self.next_session += 1;
        self.sessions.push(next);
        self.count_sessions();
    }

    fn newest_sync_arrived_by(&self, t: Option<Time>) -> Option<u64> {
        let t = t?;
        self.syncs
            .iter()
            .rev()
            .find(|&&s| self.entry(s).is_some_and(|e| e.arrival <= t))
            .copied()
    }

    fn run_pending(&mut self, now: Time) {
        loop {
            match self.out.pending {
                Pending::None => return,
                Pending::GoLiveNow { after_seq } => {
                    let Some(&k) = self.syncs.back().filter(|&&s| s > after_seq) else {
                        return;
                    };
                    if k > self.out.next_seq || self.out.need_sync {
                        let arrival = self.entry(k).map(|e| e.arrival).unwrap_or(now);
                        self.splice_to(k, now.saturating_sub(arrival));
                    } else {
                        // Already at the live edge.
                        self.out.delay = self.out.delay.min(self.out.target);
                    }
                    self.out.pending = Pending::None;
                    return;
                }
                Pending::AfterAir { mark_seq } => {
                    if self.out.next_seq > mark_seq {
                        let last = self.next_seq.saturating_sub(1);
                        self.out.pending = Pending::GoLiveNow { after_seq: last };
                        continue;
                    }
                    return;
                }
                Pending::Reduce { delay } => {
                    if self.out.delay <= delay {
                        self.out.pending = Pending::None;
                        return;
                    }
                    if let Some(k) = self.newest_sync_arrived_by(now.checked_sub(delay))
                        && k > self.out.next_seq
                    {
                        let arrival = self.entry(k).map(|e| e.arrival).unwrap_or(now);
                        self.splice_to(k, now.saturating_sub(arrival));
                        self.out.pending = Pending::None;
                    }
                    return;
                }
                Pending::Mask {
                    delay,
                    started,
                    anchor,
                } => {
                    let anchor = match anchor {
                        Some(a) => a,
                        None => {
                            let from = started + self.config.mask_margin_ms * MS;
                            let found = self
                                .syncs
                                .iter()
                                .find(|&&s| self.entry(s).is_some_and(|e| e.arrival >= from))
                                .copied();
                            match found {
                                Some(a) => {
                                    self.out.pending = Pending::Mask {
                                        delay,
                                        started,
                                        anchor: Some(a),
                                    };
                                    a
                                }
                                None => return,
                            }
                        }
                    };
                    let Some(arrival) = self.entry(anchor).map(|e| e.arrival) else {
                        // Evicted (only possible with a tiny RAM cap): give up on the mask.
                        self.set_pending(Pending::None);
                        return;
                    };
                    if now >= arrival + delay {
                        self.splice_to(anchor, now - arrival);
                        self.set_pending(Pending::None);
                    }
                    return;
                }
                Pending::Dump { delay, started } => {
                    // The first keyframe recorded once the slate is surely up.
                    let from = started + self.config.mask_margin_ms * MS;
                    let Some(anchor) = self
                        .syncs
                        .iter()
                        .find(|&&s| self.entry(s).is_some_and(|e| e.arrival >= from))
                        .copied()
                    else {
                        return;
                    };
                    let arrival = self.entry(anchor).map_or(now, |e| e.arrival);
                    // What the slate covers airs now, and the delay builds back up
                    // behind it as in mask mode (the slate stays up).
                    self.splice_to(anchor, now.saturating_sub(arrival));
                    self.out.pending = Pending::Mask {
                        delay,
                        started,
                        anchor: Some(anchor),
                    };
                }
                Pending::Replay { from, delay } => {
                    let Some(k) = self.syncs.iter().copied().find(|&s| s >= from) else {
                        return;
                    };
                    let arrival = self.entry(k).map_or(now, |e| e.arrival);
                    if now >= arrival + delay {
                        self.splice_to(k, now - arrival);
                        self.out.pending = Pending::None;
                    }
                    return;
                }
            }
        }
    }

    // ----- output ---------------------------------------------------------------

    /// Moves every message that is due into `out` and returns when to call again.
    pub fn poll(&mut self, now: Time, out: &mut Vec<OutMsg>) -> Option<Time> {
        self.poll_budget(now, out, usize::MAX)
    }

    /// Like [`Engine::poll`], but stops once `max_bytes` of payload have been
    /// moved into `out` (it may go over by one message). Used when the destination
    /// is falling behind: what is held back stays in the buffer and airs later,
    /// never sooner.
    pub fn poll_budget(
        &mut self,
        now: Time,
        out: &mut Vec<OutMsg>,
        max_bytes: usize,
    ) -> Option<Time> {
        self.evict(now);
        self.run_pending(now);
        let pending_wake = (self.out.pending != Pending::None).then_some(now + 50 * MS);
        if !self.out.connected {
            return pending_wake;
        }
        let mut wake = None;
        let mut emitted = 0usize;
        loop {
            // After a dump, nothing airs until a keyframe recorded under the slate.
            if matches!(self.out.pending, Pending::Dump { .. }) {
                break;
            }
            // Nor, after a replay, until the keyframe it continues from is due.
            if let Pending::Replay { from, .. } = self.out.pending
                && self.out.next_seq >= from
            {
                break;
            }
            if self.out.next_seq < self.base_seq {
                self.out.next_seq = self.base_seq;
                self.out.need_sync = true;
            }
            if self.out.end_mark.is_some_and(|m| self.out.next_seq > m) {
                break;
            }
            if let Some(cs) = self.out.current_session
                && !self.out.need_sync
                && let Some(e) = self.entry(self.out.next_seq)
                && e.session != cs
            {
                // The encoder reconnected: restart at the new session's first keyframe.
                self.out.need_sync = true;
            }
            if self.out.need_sync {
                let session = self.entry(self.out.next_seq).map(|e| e.session);
                let mut candidates = self.syncs.iter().copied().filter(|&s| {
                    s >= self.out.next_seq && self.entry(s).map(|e| e.session) == session
                });
                let first = candidates.next();
                let k = if self.out.started {
                    first
                } else {
                    // Initial start (for example a destination that connected late):
                    // begin at the newest keyframe that already meets the delay.
                    let delay = self.out.delay;
                    first.map(|f| {
                        std::iter::once(f)
                            .chain(candidates)
                            .filter(|&s| self.entry(s).is_some_and(|e| e.arrival + delay <= now))
                            .last()
                            .unwrap_or(f)
                    })
                };
                let Some(k) = k else {
                    // Nothing to start from yet; skip what cannot be decoded.
                    if let Some(s) = session
                        && self.sessions.last().map(|x| x.id) != Some(s)
                    {
                        // An old session without a keyframe left: move on.
                        self.out.next_seq = self.next_session_start(s);
                        continue;
                    }
                    break;
                };
                if let Some(m) = self.out.end_mark
                    && k > m
                {
                    // Nothing decodable is left before the end: skip to it.
                    self.out.next_seq = m + 1;
                    break;
                }
                let arrival = self.entry(k).map(|e| e.arrival).unwrap_or(now);
                if now < arrival + self.out.delay {
                    wake = Some(arrival + self.out.delay);
                    break;
                }
                let delay = self.out.delay.max(now - arrival);
                self.splice_to(k, delay);
                if self.out.end_mark.is_some_and(|m| self.out.next_seq > m) {
                    break;
                }
            }
            let Some(e) = self.entry(self.out.next_seq) else {
                break;
            };
            let due = e.arrival + self.out.delay;
            if now < due {
                wake = Some(due);
                break;
            }
            if emitted >= max_bytes {
                // Look again soon; the caller passes a new budget once it has room.
                wake = Some(now + 50 * MS);
                break;
            }
            let before = out.len();
            if self.out.pending_headers {
                self.emit_headers(out);
            }
            self.out.first_emitted.get_or_insert(self.out.next_seq);
            self.emit_entry(out);
            emitted += out[before..].iter().map(|m| m.payload.len()).sum::<usize>();
            self.out.next_seq += 1;
            self.run_pending(now);
        }
        match (wake, pending_wake) {
            (Some(a), Some(b)) => Some(a.min(b)),
            (a, b) => a.or(b),
        }
    }

    fn next_session_start(&self, session: u32) -> u64 {
        self.ring
            .iter()
            .find(|e| e.session > session)
            .map(|e| e.seq)
            .unwrap_or(self.next_seq)
    }

    fn entry(&self, seq: u64) -> Option<&Entry> {
        if seq < self.base_seq {
            return None;
        }
        self.ring.get((seq - self.base_seq) as usize)
    }

    fn splice_to(&mut self, k: u64, delay: u64) {
        let Some(e) = self.entry(k) else { return };
        let (ts, session, cts) = (e.ts, e.session, e.cts);
        let cra = e.hevc_nal == Some(flv::hevc::CRA);
        let frame = self
            .sessions
            .iter()
            .find(|s| s.id == session)
            .map_or(33, |s| s.frame_ms);
        let o = &mut self.out;
        // The keyframe must follow everything already sent both in decode order (DTS)
        // and in presentation order (PTS): frames cut off before the splice may have
        // presentation times later than their decode times (B-frames).
        let base = if o.fresh {
            0
        } else {
            let after_dts = o.last_out_ts + frame.max(1);
            let after_pts = o.last_pts + frame.max(1) as i64 - cts as i64;
            after_dts.max(after_pts.max(0) as u64)
        };
        o.ts_offset = base as i64 - ts as i64;
        o.fresh = false;
        o.gate = Some((session, ts));
        o.skip_rasl_after = cra.then_some(k);
        if o.started {
            o.splices += 1;
        }
        o.started = true;
        o.next_seq = k;
        o.delay = delay;
        o.need_sync = false;
        o.current_session = Some(session);
        o.pending_headers = true;
    }

    fn out_ts(&mut self, kind: Kind, ts: u64) -> u64 {
        let o = &mut self.out;
        let mut t = (ts as i64 + o.ts_offset).max(0) as u64;
        if let Some(last) = o.last_kind_ts[kind.index()] {
            t = t.max(last);
        }
        o.last_kind_ts[kind.index()] = Some(t);
        o.last_out_ts = o.last_out_ts.max(t);
        t
    }

    /// Sends metadata and decoder configuration valid at the splice point, if the
    /// destination has not seen them yet.
    fn emit_headers(&mut self, out: &mut Vec<OutMsg>) {
        self.out.pending_headers = false;
        let Some(e) = self.entry(self.out.next_seq) else {
            return;
        };
        let (seq, session_id, ts) = (e.seq, e.session, e.ts);
        let Some(session) = self.sessions.iter().find(|s| s.id == session_id) else {
            return;
        };
        let metadata = session.metadata.clone();
        let headers: Vec<(Kind, u16, Bytes)> = session
            .headers
            .iter()
            .filter(|h| h.seq < seq)
            .map(|h| (h.kind, h.class, h.payload.clone()))
            .collect();
        if let Some(m) = metadata
            && self.out.sent_metadata.as_ref() != Some(&m)
        {
            let t = self.out_ts(Kind::Data, ts);
            self.out.sent_metadata = Some(m.clone());
            out.push(OutMsg {
                kind: Kind::Data,
                timestamp: t as u32,
                payload: m,
                seq: None,
            });
        }
        for (kind, class, payload) in headers {
            if self.mark_header_sent(kind, class, &payload) {
                let t = self.out_ts(kind, ts);
                self.out.sent_bytes += payload.len() as u64;
                out.push(OutMsg {
                    kind,
                    timestamp: t as u32,
                    payload,
                    seq: None,
                });
            }
        }
    }

    /// Records a header as sent. Returns false if the destination already has it.
    fn mark_header_sent(&mut self, kind: Kind, class: u16, payload: &Bytes) -> bool {
        let sent = &mut self.out.sent_headers;
        if sent
            .iter()
            .any(|(k, c, p)| *k == kind && *c == class && p == payload)
        {
            return false;
        }
        sent.retain(|(k, c, _)| !(*k == kind && *c == class));
        sent.push((kind, class, payload.clone()));
        true
    }

    fn emit_entry(&mut self, out: &mut Vec<OutMsg>) {
        let Some(e) = self.entry(self.out.next_seq) else {
            return;
        };
        let (seq, kind, ts, session, config) = (e.seq, e.kind, e.ts, e.session, e.config);
        let payload = e.payload.clone();
        if let Some(class) = config {
            if self.mark_header_sent(kind, class, &payload) {
                let t = self.out_ts(kind, ts);
                self.out.sent_bytes += payload.len() as u64;
                out.push(OutMsg {
                    kind,
                    timestamp: t as u32,
                    payload,
                    seq: Some(seq),
                });
            }
            return;
        }
        if kind == Kind::Video
            && let Some(k) = self.out.skip_rasl_after
            && let Some(t) = self.entry(seq).and_then(|e| e.hevc_nal)
        {
            if seq > k && (flv::hevc::is_trailing(t) || flv::hevc::is_irap(t)) {
                self.out.skip_rasl_after = None;
            } else if flv::hevc::is_rasl(t) {
                self.out.dropped += 1;
                return;
            }
        }
        if kind != Kind::Video
            && let Some((gs, gts)) = self.out.gate
            && gs == session
            && ts < gts
        {
            self.out.dropped += 1;
            return;
        }
        let mut payload = payload;
        if self.out.skip_rasl_after == Some(seq)
            && let Some(offset) = self.entry(seq).and_then(|e| e.nal_offset)
            && let Some(bla) = flv::hevc::cra_to_bla(&payload, offset)
        {
            // Splicing onto an open-GOP keyframe: mark the broken link.
            payload = Bytes::from(bla);
        }
        let t = self.out_ts(kind, ts);
        if kind == Kind::Video {
            let cts = self.entry(seq).map_or(0, |e| e.cts);
            self.out.last_pts = self.out.last_pts.max(t as i64 + cts as i64);
        }
        self.out.sent_bytes += payload.len() as u64;
        out.push(OutMsg {
            kind,
            timestamp: t as u32,
            payload,
            seq: Some(seq),
        });
    }

    // ----- buffer management ----------------------------------------------------

    /// Without history, the oldest entry that must stay: the keyframe the output
    /// would restart from after a reconnect (or, before the broadcast starts, the
    /// one it would start from), and a pending mask's rewind point.
    fn keep_from(&self, now: Time) -> Option<u64> {
        if self.config.keep_history {
            return None;
        }
        let o = &self.out;
        let mut keep = if o.started {
            self.syncs
                .iter()
                .rev()
                .find(|&&s| s <= o.next_seq)
                .copied()
                .unwrap_or(o.next_seq)
        } else {
            self.newest_sync_arrived_by(now.checked_sub(o.delay))?
        };
        if let Pending::Mask {
            anchor: Some(a), ..
        } = o.pending
        {
            keep = keep.min(a);
        }
        Some(keep)
    }

    fn evict(&mut self, now: Time) {
        let capacity = (self.config.max_delay_ms + self.config.headroom_ms) * MS;
        let keep_from = self.keep_from(now);
        while let Some(front) = self.ring.front() {
            let over_ram =
                self.bytes + self.session_bytes > self.config.ram_cap_bytes && self.ring.len() > 1;
            // Evict a whole GOP once its successor keyframe is older than the capacity,
            // so the oldest entry is always a keyframe.
            let next_sync = self.syncs.iter().find(|&&s| s > front.seq).copied();
            let gop_expired = match next_sync {
                Some(s) => self.entry(s).is_some_and(|e| e.arrival + capacity < now),
                None => !front.sync && front.arrival + capacity < now,
            };
            // Without history, a GOP goes as soon as nothing needs it any more.
            let not_needed = keep_from.zip(next_sync).is_some_and(|(k, s)| s <= k);
            if !over_ram && !gop_expired && !not_needed {
                break;
            }
            let end = next_sync.unwrap_or(front.seq + 1);
            while let Some(f) = self.ring.front() {
                if f.seq >= end {
                    break;
                }
                let f = self.ring.pop_front().expect("front exists");
                self.bytes -= cost(f.payload.len());
                self.base_seq = f.seq + 1;
                if self.syncs.front() == Some(&f.seq) {
                    self.syncs.pop_front();
                }
            }
            self.forget_sessions();
        }
    }

    /// Forgets the sessions that no longer have buffered content. Sessions after
    /// the oldest buffered message's all have some (see [`Engine::ingest_start`]),
    /// except perhaps the current one.
    fn forget_sessions(&mut self) {
        let Some(front) = self.ring.front() else {
            return;
        };
        let oldest = front.session;
        if self.sessions.first().is_some_and(|s| s.id < oldest) {
            let current = self.sessions.last().map(|s| s.id);
            self.sessions
                .retain(|s| s.id >= oldest || Some(s.id) == current);
            self.count_sessions();
        }
    }

    /// Current state for the API and UI.
    pub fn snapshot(&self, now: Time) -> Snapshot {
        snapshot::build(self, now)
    }
}

#[cfg(test)]
mod tests;
