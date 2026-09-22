//! Read-only inspection of the audio/video message bodies carried over RTMP.
//!
//! Handles legacy FLV tags (AVC/AAC and friends) and Enhanced RTMP v1/v2 headers
//! (FourCC codecs, ModEx, multitrack). Payloads are never modified: the relay only
//! needs to know which messages are keyframes and which carry decoder configuration.

use serde::Serialize;

/// Video codec, from either a legacy codec id or an Enhanced RTMP FourCC.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum VideoCodec {
    Avc,
    Hevc,
    Av1,
    Vp9,
    Vp8,
    Other,
}

/// Audio codec, from either a legacy sound format or an Enhanced RTMP FourCC.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum AudioCodec {
    Aac,
    Mp3,
    Opus,
    Flac,
    Ac3,
    Eac3,
    Other,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct VideoInfo {
    pub codec: VideoCodec,
    /// A frame a decoder can start from (IDR / key frame with coded data).
    pub keyframe: bool,
    /// Decoder configuration, identified by [`VideoInfo::config_class`].
    pub config: bool,
    /// Distinguishes kinds of configuration messages so each can be cached separately.
    pub config_class: u16,
    pub enhanced: bool,
    pub multitrack: bool,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct AudioInfo {
    pub codec: AudioCodec,
    pub config: bool,
    pub config_class: u16,
    pub enhanced: bool,
    pub multitrack: bool,
}

mod frame_type {
    pub const KEY: u8 = 1;
    pub const COMMAND: u8 = 5;
}

mod video_packet {
    pub const SEQUENCE_START: u8 = 0;
    pub const CODED_FRAMES: u8 = 1;
    pub const CODED_FRAMES_X: u8 = 3;
    pub const METADATA: u8 = 4;
    pub const MPEG2TS_SEQUENCE_START: u8 = 5;
    pub const MULTITRACK: u8 = 6;
    pub const MOD_EX: u8 = 7;
}

mod audio_packet {
    pub const SEQUENCE_START: u8 = 0;
    pub const MULTICHANNEL_CONFIG: u8 = 4;
    pub const MULTITRACK: u8 = 5;
    pub const MOD_EX: u8 = 7;
}

struct Reader<'a> {
    b: &'a [u8],
    pos: usize,
}

impl Reader<'_> {
    fn u8(&mut self) -> Option<u8> {
        let v = *self.b.get(self.pos)?;
        self.pos += 1;
        Some(v)
    }

    fn u16(&mut self) -> Option<u16> {
        Some(u16::from_be_bytes([self.u8()?, self.u8()?]))
    }

    fn fourcc(&mut self) -> Option<[u8; 4]> {
        let s = self.b.get(self.pos..self.pos + 4)?;
        self.pos += 4;
        Some([s[0], s[1], s[2], s[3]])
    }

    fn skip(&mut self, n: usize) -> Option<()> {
        if self.pos + n > self.b.len() {
            return None;
        }
        self.pos += n;
        Some(())
    }

    /// Skips ModEx blocks and returns the final packet type.
    fn skip_mod_ex(&mut self, mut packet_type: u8, mod_ex: u8) -> Option<u8> {
        let mut guard = 0;
        while packet_type == mod_ex {
            guard += 1;
            if guard > 16 {
                return None;
            }
            let mut size = self.u8()? as usize + 1;
            if size == 256 {
                size = self.u16()? as usize + 1;
            }
            self.skip(size)?;
            packet_type = self.u8()? & 0x0f;
        }
        Some(packet_type)
    }
}

fn video_codec_from_fourcc(f: [u8; 4]) -> VideoCodec {
    match &f {
        b"avc1" => VideoCodec::Avc,
        b"hvc1" => VideoCodec::Hevc,
        b"av01" => VideoCodec::Av1,
        b"vp09" => VideoCodec::Vp9,
        b"vp08" => VideoCodec::Vp8,
        _ => VideoCodec::Other,
    }
}

fn audio_codec_from_fourcc(f: [u8; 4]) -> AudioCodec {
    match &f {
        b"mp4a" => AudioCodec::Aac,
        b".mp3" => AudioCodec::Mp3,
        b"Opus" => AudioCodec::Opus,
        b"fLaC" => AudioCodec::Flac,
        b"ac-3" => AudioCodec::Ac3,
        b"ec-3" => AudioCodec::Eac3,
        _ => AudioCodec::Other,
    }
}

/// Inspects a video message body. Returns `None` for empty or truncated bodies.
pub fn inspect_video(payload: &[u8]) -> Option<VideoInfo> {
    let mut r = Reader { b: payload, pos: 0 };
    let b0 = r.u8()?;
    if b0 & 0x80 == 0 {
        // Legacy FLV video tag.
        let ft = b0 >> 4;
        let codec = match b0 & 0x0f {
            7 => VideoCodec::Avc,
            12 => VideoCodec::Hevc,
            13 => VideoCodec::Av1,
            _ => VideoCodec::Other,
        };
        let has_packet_type = codec != VideoCodec::Other;
        let packet_type = if has_packet_type { r.u8()? } else { 1 };
        let config = has_packet_type && packet_type == 0;
        return Some(VideoInfo {
            codec,
            keyframe: ft == frame_type::KEY && packet_type == 1,
            config,
            config_class: 0,
            enhanced: false,
            multitrack: false,
        });
    }

    let ft = (b0 >> 4) & 0x07;
    let mut packet_type = r.skip_mod_ex(b0 & 0x0f, video_packet::MOD_EX)?;
    if packet_type != video_packet::METADATA && ft == frame_type::COMMAND {
        // Command frames (seek start/end) carry no coded data.
        return Some(VideoInfo {
            codec: VideoCodec::Other,
            keyframe: false,
            config: false,
            config_class: 0,
            enhanced: true,
            multitrack: false,
        });
    }
    let mut multitrack = false;
    let mut track_id = 0u8;
    let fourcc;
    if packet_type == video_packet::MULTITRACK {
        multitrack = true;
        // For every multitrack layout the first track begins with a FourCC (shared or
        // per-track) followed by its track id.
        packet_type = r.u8()? & 0x0f;
        fourcc = r.fourcc()?;
        track_id = r.u8().unwrap_or(0);
    } else {
        fourcc = r.fourcc()?;
    }
    let config = matches!(
        packet_type,
        video_packet::SEQUENCE_START | video_packet::MPEG2TS_SEQUENCE_START
    );
    Some(VideoInfo {
        codec: video_codec_from_fourcc(fourcc),
        keyframe: ft == frame_type::KEY
            && matches!(
                packet_type,
                video_packet::CODED_FRAMES | video_packet::CODED_FRAMES_X
            ),
        config,
        config_class: (track_id as u16) << 8 | packet_type as u16,
        enhanced: true,
        multitrack,
    })
}

/// Inspects an audio message body. Returns `None` for empty or truncated bodies.
pub fn inspect_audio(payload: &[u8]) -> Option<AudioInfo> {
    let mut r = Reader { b: payload, pos: 0 };
    let b0 = r.u8()?;
    let format = b0 >> 4;
    if format != 9 {
        let codec = match format {
            10 => AudioCodec::Aac,
            2 | 14 => AudioCodec::Mp3,
            _ => AudioCodec::Other,
        };
        let config = codec == AudioCodec::Aac && r.u8()? == 0;
        return Some(AudioInfo {
            codec,
            config,
            config_class: 0,
            enhanced: false,
            multitrack: false,
        });
    }

    let mut packet_type = r.skip_mod_ex(b0 & 0x0f, audio_packet::MOD_EX)?;
    let mut multitrack = false;
    let mut track_id = 0u8;
    let fourcc;
    if packet_type == audio_packet::MULTITRACK {
        multitrack = true;
        let b = r.u8()?;
        packet_type = b & 0x0f;
        fourcc = r.fourcc()?;
        track_id = r.u8().unwrap_or(0);
    } else {
        fourcc = r.fourcc()?;
    }
    let config = matches!(
        packet_type,
        audio_packet::SEQUENCE_START | audio_packet::MULTICHANNEL_CONFIG
    );
    Some(AudioInfo {
        codec: audio_codec_from_fourcc(fourcc),
        config,
        config_class: (track_id as u16) << 8 | packet_type as u16,
        enhanced: true,
        multitrack,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn legacy_avc() {
        let seq = inspect_video(&[0x17, 0x00, 0, 0, 0, 1, 0x64]).unwrap();
        assert!(seq.config && !seq.keyframe && seq.codec == VideoCodec::Avc);
        let idr = inspect_video(&[0x17, 0x01, 0, 0, 0, 0, 0, 0, 1]).unwrap();
        assert!(idr.keyframe && !idr.config);
        let p = inspect_video(&[0x27, 0x01, 0, 0, 0]).unwrap();
        assert!(!p.keyframe && !p.config);
        assert!(inspect_video(&[]).is_none());
        assert!(inspect_video(&[0x17]).is_none());
    }

    #[test]
    fn legacy_aac() {
        let a = inspect_audio(&[0xaf, 0x00, 0x12, 0x10]).unwrap();
        assert!(a.config && a.codec == AudioCodec::Aac);
        let a = inspect_audio(&[0xaf, 0x01, 0x21]).unwrap();
        assert!(!a.config);
        let mp3 = inspect_audio(&[0x2f, 0xff]).unwrap();
        assert_eq!(mp3.codec, AudioCodec::Mp3);
    }

    #[test]
    fn enhanced_hevc_and_av1() {
        // IsExHeader | key frame | SequenceStart, 'hvc1'
        let mut seq = vec![0x80 | 0x10];
        seq.extend_from_slice(b"hvc1");
        let i = inspect_video(&seq).unwrap();
        assert!(i.enhanced && i.config && i.codec == VideoCodec::Hevc);
        // key frame | CodedFramesX, 'av01'
        let mut kf = vec![0x80 | 0x10 | 0x03];
        kf.extend_from_slice(b"av01");
        kf.push(0);
        let i = inspect_video(&kf).unwrap();
        assert!(i.keyframe && !i.config && i.codec == VideoCodec::Av1);
        // inter frame | CodedFrames
        let mut inter = vec![0x80 | 0x20 | 0x01];
        inter.extend_from_slice(b"hvc1");
        assert!(!inspect_video(&inter).unwrap().keyframe);
    }

    #[test]
    fn enhanced_mod_ex_and_multitrack() {
        // ModEx wrapping a CodedFrames packet: size-1 = 2 (3 bytes of data), then
        // modExType|packetType byte.
        let mut v = vec![0x80 | 0x10 | 0x07, 2, 9, 9, 9, 0x01];
        v.extend_from_slice(b"avc1");
        let i = inspect_video(&v).unwrap();
        assert!(i.keyframe && i.codec == VideoCodec::Avc);

        // Multitrack, OneTrack, SequenceStart, 'hvc1', track 1
        let mut m = vec![0x80 | 0x10 | 0x06, 0x00];
        m.extend_from_slice(b"hvc1");
        m.push(1);
        let i = inspect_video(&m).unwrap();
        assert!(i.multitrack && i.config && i.config_class == (1 << 8));
    }

    #[test]
    fn enhanced_opus_and_command_frames() {
        let mut a = vec![0x90];
        a.extend_from_slice(b"Opus");
        let i = inspect_audio(&a).unwrap();
        assert!(i.config && i.codec == AudioCodec::Opus && i.enhanced);
        let mut a = vec![0x91];
        a.extend_from_slice(b"Opus");
        assert!(!inspect_audio(&a).unwrap().config);

        let cmd = inspect_video(&[0x80 | 0x50 | 0x01, 0x00]).unwrap();
        assert!(!cmd.keyframe && !cmd.config);
    }
}
