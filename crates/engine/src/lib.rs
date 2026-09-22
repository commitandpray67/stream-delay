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
}

impl Default for EngineConfig {
    fn default() -> Self {
        Self {
            max_delay_ms: 120_000,
            headroom_ms: 10_000,
            ram_cap_bytes: 512 * 1024 * 1024,
            mask_margin_ms: 500,
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

#[derive(Debug, Error, PartialEq, Eq)]
pub enum EngineError {
    #[error("requested delay of {requested_ms} ms exceeds the maximum of {max_ms} ms")]
    TooLarge { requested_ms: u64, max_ms: u64 },
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
    sessions: Vec<Session>,
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

    // ----- ingest ---------------------------------------------------------------

    /// A publisher connected. Returns the new session id.
    pub fn ingest_start(&mut self, _now: Time) -> u32 {
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

    /// Stream metadata (`@setDataFrame onMetaData ...`).
    pub fn ingest_metadata(&mut self, _now: Time, payload: Bytes) {
        if let Some(s) = self.sessions.last_mut() {
            s.metadata = Some(payload);
        }
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
        if let Some(class) = config {
            session
                .headers
                .retain(|h| !(h.kind == kind && h.class == class));
            session.headers.push(HeaderRec {
                seq,
                kind,
                class,
                payload: payload.clone(),
            });
        }
        if sync {
            self.syncs.push_back(seq);
        }
        self.stats.bytes(now, payload.len());
        self.bytes += payload.len();
        let session = session.id;
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
        if matches!(o.pending, Pending::Mask { .. }) {
            o.mask_visible = false;
        }
        o.pending = Pending::None;
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

    // ----- commands -------------------------------------------------------------

    pub fn command(&mut self, now: Time, cmd: Command) -> Result<Ack, EngineError> {
        match cmd {
            Command::Cancel => {
                self.set_pending(Pending::None);
                self.out.target = self.out.delay;
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

    fn ack(&self) -> Ack {
        Ack {
            target_ms: self.out.target / MS,
            effective_ms: self.out.delay / MS,
            pending: self.out.pending != Pending::None,
            history_short: self.out.history_short,
        }
    }

    fn set_pending(&mut self, p: Pending) {
        if matches!(self.out.pending, Pending::Mask { .. }) {
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
            }
        }
    }

    // ----- output ---------------------------------------------------------------

    /// Moves every message that is due into `out` and returns when to call again.
    pub fn poll(&mut self, now: Time, out: &mut Vec<OutMsg>) -> Option<Time> {
        self.evict(now);
        self.run_pending(now);
        let pending_wake = (self.out.pending != Pending::None).then_some(now + 50 * MS);
        if !self.out.connected {
            return pending_wake;
        }
        let mut wake = None;
        loop {
            if self.out.next_seq < self.base_seq {
                self.out.next_seq = self.base_seq;
                self.out.need_sync = true;
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
                let arrival = self.entry(k).map(|e| e.arrival).unwrap_or(now);
                if now < arrival + self.out.delay {
                    wake = Some(arrival + self.out.delay);
                    break;
                }
                let delay = self.out.delay.max(now - arrival);
                self.splice_to(k, delay);
            }
            let Some(e) = self.entry(self.out.next_seq) else {
                break;
            };
            let due = e.arrival + self.out.delay;
            if now < due {
                wake = Some(due);
                break;
            }
            if self.out.pending_headers {
                self.emit_headers(out);
            }
            self.out.first_emitted.get_or_insert(self.out.next_seq);
            self.emit_entry(out);
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

    fn evict(&mut self, now: Time) {
        let capacity = (self.config.max_delay_ms + self.config.headroom_ms) * MS;
        loop {
            let Some(front) = self.ring.front() else {
                break;
            };
            let over_ram = self.bytes > self.config.ram_cap_bytes && self.ring.len() > 1;
            // Evict a whole GOP once its successor keyframe is older than the capacity,
            // so the oldest entry is always a keyframe.
            let next_sync = self.syncs.iter().find(|&&s| s > front.seq).copied();
            let gop_expired = match next_sync {
                Some(s) => self.entry(s).is_some_and(|e| e.arrival + capacity < now),
                None => !front.sync && front.arrival + capacity < now,
            };
            if !over_ram && !gop_expired {
                break;
            }
            let end = next_sync.unwrap_or(front.seq + 1);
            while let Some(f) = self.ring.front() {
                if f.seq >= end {
                    break;
                }
                let f = self.ring.pop_front().expect("front exists");
                self.bytes -= f.payload.len();
                self.base_seq = f.seq + 1;
                if self.syncs.front() == Some(&f.seq) {
                    self.syncs.pop_front();
                }
            }
        }
        // Forget sessions that no longer have buffered content.
        if let Some(front) = self.ring.front() {
            let oldest = front.session;
            let current = self.sessions.last().map(|s| s.id);
            self.sessions
                .retain(|s| s.id >= oldest || Some(s.id) == current);
        }
    }

    /// Current state for the API and UI.
    pub fn snapshot(&self, now: Time) -> Snapshot {
        snapshot::build(self, now)
    }
}

#[cfg(test)]
mod tests;
