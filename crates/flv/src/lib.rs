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
    /// Composition time offset (PTS - DTS) in milliseconds, for codecs with B-frames.
    pub composition_time: i32,
    /// For HEVC: NAL unit type of the first coded slice (IDR, CRA, RASL, ...).
    pub hevc_nal_type: Option<u8>,
    /// For HEVC: offset of the length-prefixed NAL unit data in the payload.
    pub nal_offset: Option<usize>,
}

/// HEVC NAL unit types relevant to splicing (ITU-T H.265 table 7-1).
pub mod hevc {
    pub const RASL_N: u8 = 8;
    pub const RASL_R: u8 = 9;
    pub const CRA: u8 = 21;

    /// Random-access skipped leading picture: references frames before its CRA.
    pub fn is_rasl(t: u8) -> bool {
        t == RASL_N || t == RASL_R
    }

    /// Intra random access point (BLA, IDR or CRA).
    pub fn is_irap(t: u8) -> bool {
        (16..=23).contains(&t)
    }

    /// A picture that follows its IRAP in output order.
    pub fn is_trailing(t: u8) -> bool {
        t <= 5
    }

    pub const BLA_W_LP: u8 = 16;

    /// Rewrites every CRA slice in `payload` (length-prefixed NAL units starting at
    /// `offset`) as BLA_W_LP. A CRA in the middle of a stream does not reset the
    /// decoder; a BLA marks a broken link so the decoder discards leading pictures
    /// and restarts output order. This is the standard way to splice open-GOP HEVC.
    pub fn cra_to_bla(payload: &[u8], offset: usize) -> Option<Vec<u8>> {
        let mut out = payload.to_vec();
        let mut pos = offset;
        let mut changed = false;
        while pos + 5 <= out.len() {
            let len =
                u32::from_be_bytes([out[pos], out[pos + 1], out[pos + 2], out[pos + 3]]) as usize;
            let header = pos + 4;
            if len == 0 || header + len > out.len() {
                break;
            }
            if (out[header] >> 1) & 0x3f == CRA {
                out[header] = (out[header] & 0x81) | (BLA_W_LP << 1);
                changed = true;
            }
            pos = header + len;
        }
        changed.then_some(out)
    }
}

/// Returns the NAL type of the first coded slice in length-prefixed HEVC data.
fn first_hevc_vcl(mut data: &[u8]) -> Option<u8> {
    while data.len() >= 6 {
        let len = u32::from_be_bytes([data[0], data[1], data[2], data[3]]) as usize;
        let nal = data.get(4..4 + len)?;
        let t = (nal.first()? >> 1) & 0x3f;
        if t < 32 {
            return Some(t);
        }
        data = &data[4 + len..];
    }
    None
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

    /// Signed 24-bit big-endian integer.
    fn si24(&mut self) -> Option<i32> {
        let s = self.b.get(self.pos..self.pos + 3)?;
        self.pos += 3;
        let v = (s[0] as i32) << 16 | (s[1] as i32) << 8 | s[2] as i32;
        Some((v << 8) >> 8)
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
        let composition_time = if has_packet_type && packet_type == 1 {
            r.si24().unwrap_or(0)
        } else {
            0
        };
        let nal_offset = (codec == VideoCodec::Hevc && packet_type == 1).then_some(r.pos);
        let hevc_nal_type = nal_offset.and_then(|o| first_hevc_vcl(payload.get(o..)?));
        return Some(VideoInfo {
            codec,
            keyframe: ft == frame_type::KEY && packet_type == 1,
            config,
            config_class: 0,
            enhanced: false,
            multitrack: false,
            composition_time,
            hevc_nal_type,
            nal_offset,
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
            composition_time: 0,
            hevc_nal_type: None,
            nal_offset: None,
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
    let codec = video_codec_from_fourcc(fourcc);
    // Only AVC and HEVC CodedFrames carry a composition time; CodedFramesX means 0.
    let composition_time = if packet_type == video_packet::CODED_FRAMES
        && matches!(codec, VideoCodec::Avc | VideoCodec::Hevc)
    {
        r.si24().unwrap_or(0)
    } else {
        0
    };
    let coded = matches!(
        packet_type,
        video_packet::CODED_FRAMES | video_packet::CODED_FRAMES_X
    );
    let nal_offset = (codec == VideoCodec::Hevc && coded && !multitrack).then_some(r.pos);
    let hevc_nal_type = nal_offset.and_then(|o| first_hevc_vcl(payload.get(o..)?));
    Some(VideoInfo {
        codec,
        keyframe: ft == frame_type::KEY
            && matches!(
                packet_type,
                video_packet::CODED_FRAMES | video_packet::CODED_FRAMES_X
            ),
        config,
        config_class: (track_id as u16) << 8 | packet_type as u16,
        enhanced: true,
        multitrack,
        composition_time,
        hevc_nal_type,
        nal_offset,
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
        let idr = inspect_video(&[0x17, 0x01, 0, 0, 0x42, 0, 0, 0, 1]).unwrap();
        assert!(idr.keyframe && !idr.config);
        assert_eq!(idr.composition_time, 0x42);
        let negative = inspect_video(&[0x27, 0x01, 0xff, 0xff, 0xfe]).unwrap();
        assert_eq!(negative.composition_time, -2);
        let p = inspect_video(&[0x27, 0x01, 0, 0, 0]).unwrap();
        assert!(!p.keyframe && !p.config);
        assert!(inspect_video(&[]).is_none());
        assert!(inspect_video(&[0x17]).is_none());
    }

    #[test]
    fn hevc_nal_types() {
        // Enhanced CodedFrames 'hvc1', cts 0, then an AUD (type 35) and a CRA slice.
        let mut v = vec![0x80 | 0x10 | 0x01];
        v.extend_from_slice(b"hvc1");
        v.extend_from_slice(&[0, 0, 0]);
        v.extend_from_slice(&[0, 0, 0, 3, 35 << 1, 1, 0x50]);
        v.extend_from_slice(&[0, 0, 0, 3, 21 << 1, 1, 0xaf]);
        let i = inspect_video(&v).unwrap();
        assert_eq!(i.hevc_nal_type, Some(hevc::CRA));
        let bla = hevc::cra_to_bla(&v, i.nal_offset.unwrap()).unwrap();
        assert_eq!(
            inspect_video(&bla).unwrap().hevc_nal_type,
            Some(hevc::BLA_W_LP)
        );
        // Only the NAL type bits change.
        assert_eq!(bla.iter().zip(&v).filter(|(a, b)| a != b).count(), 1);
        assert!(hevc::is_irap(21) && hevc::is_rasl(9) && hevc::is_trailing(1));
        // Legacy codec id 12, RASL_N slice.
        let legacy = [0x2c, 0x01, 0, 0, 0, 0, 0, 0, 2, 8 << 1, 1];
        assert_eq!(
            inspect_video(&legacy).unwrap().hevc_nal_type,
            Some(hevc::RASL_N)
        );
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
        inter.extend_from_slice(&[0, 0, 33]);
        let i = inspect_video(&inter).unwrap();
        assert!(!i.keyframe);
        assert_eq!(i.composition_time, 33);
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
