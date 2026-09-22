//! Read-only view of the engine for the API and UI.

use std::collections::VecDeque;

use serde::Serialize;
use streamdelay_flv::{AudioCodec, AudioInfo, VideoCodec, VideoInfo};

use crate::{Engine, MS, Pending, SEC, Time};

/// What the stream is doing, as shown to the streamer.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Default)]
#[serde(rename_all = "kebab-case")]
pub enum Phase {
    /// No encoder connected and nothing left to send.
    #[default]
    Offline,
    /// Output is at (or within half a second of) the live edge.
    Live,
    Delayed,
    /// Mask mode: the slate is up and the buffer is filling.
    Adding,
    /// Waiting for a keyframe or for buffered content to air before going live.
    GoingLive,
    /// Waiting for a keyframe to reduce the delay.
    Reducing,
}

#[derive(Debug, Clone, Serialize, Default, PartialEq)]
pub struct IngestStats {
    pub active: bool,
    pub video_codec: Option<VideoCodec>,
    pub audio_codec: Option<AudioCodec>,
    pub bitrate_kbps: u64,
    pub fps: f64,
    /// Measured keyframe interval.
    pub gop_ms: Option<u64>,
    pub enhanced: bool,
    pub multitrack: bool,
}

#[derive(Debug, Clone, Serialize, Default, PartialEq)]
pub struct OutputStats {
    pub connected: bool,
    pub splices: u64,
    /// Audio/data frames dropped at splice points to keep audio in sync.
    pub dropped_frames: u64,
    pub sent_bytes: u64,
}

#[derive(Debug, Clone, Serialize, Default, PartialEq)]
pub struct Snapshot {
    pub phase: Phase,
    pub target_ms: u64,
    pub effective_ms: u64,
    pub max_delay_ms: u64,
    /// How far back the buffer reaches.
    pub history_ms: u64,
    pub buffered_bytes: u64,
    /// The overlay should show the mask slate.
    pub mask_visible: bool,
    pub history_short: bool,
    pub ingest: IngestStats,
    pub output: OutputStats,
    pub warnings: Vec<String>,
}

/// Rolling ingest statistics.
#[derive(Default)]
pub(crate) struct IngestTracker {
    window: VecDeque<(Time, usize, bool)>,
    video_codec: Option<VideoCodec>,
    audio_codec: Option<AudioCodec>,
    last_key_ts: Option<u64>,
    gop_ms: Option<u64>,
    enhanced: bool,
    multitrack: bool,
}

const WINDOW: u64 = 2 * SEC;

impl IngestTracker {
    pub(crate) fn video(&mut self, now: Time, info: &VideoInfo, ts: u64) {
        self.video_codec = Some(info.codec)
            .filter(|c| *c != VideoCodec::Other)
            .or(self.video_codec);
        self.enhanced |= info.enhanced;
        self.multitrack |= info.multitrack;
        if info.keyframe {
            if let Some(last) = self.last_key_ts {
                self.gop_ms = Some(ts.saturating_sub(last));
            }
            self.last_key_ts = Some(ts);
        }
        if !info.config {
            self.window.push_back((now, 0, true));
        }
    }

    pub(crate) fn audio(&mut self, info: &AudioInfo) {
        self.audio_codec = Some(info.codec)
            .filter(|c| *c != AudioCodec::Other)
            .or(self.audio_codec);
        self.enhanced |= info.enhanced;
        self.multitrack |= info.multitrack;
    }

    pub(crate) fn bytes(&mut self, now: Time, n: usize) {
        self.window.push_back((now, n, false));
        while self
            .window
            .front()
            .is_some_and(|(t, _, _)| *t + WINDOW < now)
        {
            self.window.pop_front();
        }
    }

    fn stats(&self, now: Time, active: bool) -> IngestStats {
        let recent = self.window.iter().filter(|(t, _, _)| *t + WINDOW >= now);
        let (bytes, frames) = recent.fold((0usize, 0usize), |(b, f), (_, n, v)| {
            (b + n, f + usize::from(*v))
        });
        let secs = WINDOW as f64 / SEC as f64;
        IngestStats {
            active,
            video_codec: self.video_codec,
            audio_codec: self.audio_codec,
            bitrate_kbps: (bytes as f64 * 8.0 / 1000.0 / secs) as u64,
            fps: (frames as f64 / secs * 10.0).round() / 10.0,
            gop_ms: self.gop_ms,
            enhanced: self.enhanced,
            multitrack: self.multitrack,
        }
    }
}

pub(crate) fn build(e: &Engine, now: Time) -> Snapshot {
    let o = &e.out;
    let history_ms = e
        .ring
        .front()
        .map_or(0, |f| now.saturating_sub(f.arrival) / MS);
    let offline = !e.ingest_active && (e.drained() || !o.connected);
    let effective = if o.started { o.delay } else { o.target };
    let phase = match o.pending {
        Pending::Mask { .. } => Phase::Adding,
        Pending::GoLiveNow { .. } | Pending::AfterAir { .. } => Phase::GoingLive,
        Pending::Reduce { .. } => Phase::Reducing,
        Pending::None if offline => Phase::Offline,
        Pending::None if effective < 500 * MS => Phase::Live,
        Pending::None => Phase::Delayed,
    };
    let ingest = e.stats.stats(now, e.ingest_active);
    let mut warnings = Vec::new();
    if let Some(gop) = ingest.gop_ms
        && gop > 2_500
    {
        warnings.push(format!(
            "Keyframe interval is {:.1} s. Set it to 2 s in your encoder: delay changes happen on keyframes and Twitch requires 2 s.",
            gop as f64 / 1000.0
        ));
    }
    if ingest.multitrack {
        warnings.push(
            "Multitrack video (Enhanced Broadcasting) was detected. It is not supported yet; turn it off in OBS.".into(),
        );
    }
    if o.history_short {
        warnings.push(format!(
            "Only {:.0} s of the stream was buffered, so the delay is {:.0} s instead of {:.0} s.",
            history_ms as f64 / 1000.0,
            effective as f64 / 1e6,
            o.target as f64 / 1e6
        ));
    }
    Snapshot {
        phase,
        target_ms: o.target / MS,
        effective_ms: effective / MS,
        max_delay_ms: e.config.max_delay_ms,
        history_ms,
        buffered_bytes: e.bytes as u64,
        mask_visible: o.mask_visible,
        history_short: o.history_short,
        ingest,
        output: OutputStats {
            connected: o.connected,
            splices: o.splices,
            dropped_frames: o.dropped,
            sent_bytes: o.sent_bytes,
        },
        warnings,
    }
}
