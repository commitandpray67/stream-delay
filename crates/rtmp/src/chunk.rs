//! RTMP chunk stream codec.
//!
//! The decoder implements every header format (0–3), 1–3 byte basic headers,
//! extended timestamps (including the repeated field on type-3 continuation
//! chunks) and runtime chunk-size changes. The encoder always writes a type-0
//! header for the first chunk of a message, which every RTMP peer accepts.

use std::collections::HashMap;

use bytes::{Buf, BufMut, Bytes, BytesMut};
use thiserror::Error;

pub const DEFAULT_CHUNK_SIZE: usize = 128;
/// Largest chunk size accepted from a peer (the spec allows up to 2^31-1; we cap it).
pub const MAX_CHUNK_SIZE: usize = 1 << 24;
const EXTENDED: u32 = 0x00FF_FFFF;

/// A complete RTMP message.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Message {
    pub csid: u32,
    /// Absolute timestamp in milliseconds (wraps at 2^32).
    pub timestamp: u32,
    pub type_id: u8,
    pub stream_id: u32,
    pub payload: Bytes,
}

#[derive(Debug, Error, PartialEq, Eq)]
pub enum ChunkError {
    #[error("chunk for stream {0} uses a compressed header before any full header")]
    MissingHeader(u32),
    #[error("invalid chunk size {0}")]
    InvalidChunkSize(u32),
}

#[derive(Default)]
struct StreamState {
    timestamp: u32,
    delta: u32,
    length: u32,
    type_id: u8,
    stream_id: u32,
    extended: bool,
    ext_value: u32,
    partial: BytesMut,
    in_progress: bool,
    has_header: bool,
}

/// Incremental decoder: push received bytes, then pull complete messages.
pub struct ChunkDecoder {
    chunk_size: usize,
    streams: HashMap<u32, StreamState>,
    buf: BytesMut,
}

impl Default for ChunkDecoder {
    fn default() -> Self {
        Self::new()
    }
}

impl ChunkDecoder {
    pub fn new() -> Self {
        Self {
            chunk_size: DEFAULT_CHUNK_SIZE,
            streams: HashMap::new(),
            buf: BytesMut::new(),
        }
    }

    pub fn push(&mut self, data: &[u8]) {
        self.buf.extend_from_slice(data);
    }

    pub fn chunk_size(&self) -> usize {
        self.chunk_size
    }

    pub fn set_chunk_size(&mut self, size: u32) -> Result<(), ChunkError> {
        let s = size as usize;
        if s == 0 || s > MAX_CHUNK_SIZE {
            return Err(ChunkError::InvalidChunkSize(size));
        }
        self.chunk_size = s;
        Ok(())
    }

    /// Drops a partially received message (the Abort protocol message).
    pub fn abort(&mut self, csid: u32) {
        if let Some(s) = self.streams.get_mut(&csid) {
            s.partial.clear();
            s.in_progress = false;
        }
    }

    /// Returns the next complete message, or `None` if more bytes are needed.
    pub fn next_message(&mut self) -> Result<Option<Message>, ChunkError> {
        loop {
            match self.next_chunk()? {
                ChunkResult::NeedMore => return Ok(None),
                ChunkResult::Partial => continue,
                ChunkResult::Complete(m) => return Ok(Some(m)),
            }
        }
    }

    fn next_chunk(&mut self) -> Result<ChunkResult, ChunkError> {
        let b = &self.buf[..];
        if b.is_empty() {
            return Ok(ChunkResult::NeedMore);
        }
        let fmt = b[0] >> 6;
        let (csid, mut pos) = match b[0] & 0x3f {
            0 => {
                if b.len() < 2 {
                    return Ok(ChunkResult::NeedMore);
                }
                (64 + b[1] as u32, 2)
            }
            1 => {
                if b.len() < 3 {
                    return Ok(ChunkResult::NeedMore);
                }
                (64 + b[1] as u32 + ((b[2] as u32) << 8), 3)
            }
            n => (n as u32, 1),
        };
        let header_len = match fmt {
            0 => 11,
            1 => 7,
            2 => 3,
            _ => 0,
        };
        if b.len() < pos + header_len {
            return Ok(ChunkResult::NeedMore);
        }
        let h = &b[pos..pos + header_len];
        pos += header_len;

        let default_state = StreamState::default();
        let st = self.streams.get(&csid).unwrap_or(&default_state);
        if fmt != 0 && !st.has_header {
            return Err(ChunkError::MissingHeader(csid));
        }

        let mut ts_field = 0u32;
        let mut length = st.length;
        let mut type_id = st.type_id;
        let mut stream_id = st.stream_id;
        if fmt <= 2 {
            ts_field = u24(&h[0..3]);
        }
        if fmt <= 1 {
            length = u24(&h[3..6]);
            type_id = h[6];
        }
        if fmt == 0 {
            stream_id = u32::from_le_bytes([h[7], h[8], h[9], h[10]]);
        }

        // A header chunk (fmt 0-2) always starts a new message, discarding any
        // partial one; a type-3 chunk continues the partial message if there is one.
        let continuation = fmt == 3 && st.in_progress;
        let mut extended = st.extended;
        let mut ext_value = st.ext_value;
        if fmt <= 2 {
            extended = ts_field == EXTENDED;
            if extended {
                if b.len() < pos + 4 {
                    return Ok(ChunkResult::NeedMore);
                }
                ext_value = be32(&b[pos..pos + 4]);
                pos += 4;
            }
        } else if extended {
            if continuation {
                // Most encoders (librtmp/OBS, FFmpeg) repeat the extended timestamp on
                // continuation chunks, some do not. Consume it only if it matches.
                if b.len() < pos + 4 {
                    return Ok(ChunkResult::NeedMore);
                }
                if be32(&b[pos..pos + 4]) == st.ext_value {
                    pos += 4;
                }
            } else {
                if b.len() < pos + 4 {
                    return Ok(ChunkResult::NeedMore);
                }
                ext_value = be32(&b[pos..pos + 4]);
                pos += 4;
            }
        }

        let already = if continuation { st.partial.len() } else { 0 };
        let remaining = (length as usize).saturating_sub(already);
        let take = remaining.min(self.chunk_size);
        if b.len() < pos + take {
            return Ok(ChunkResult::NeedMore);
        }

        // The whole chunk is available: commit state.
        let raw_ts = if extended { ext_value } else { ts_field };
        let st = self.streams.entry(csid).or_default();
        if !continuation {
            match fmt {
                0 => {
                    st.timestamp = raw_ts;
                    st.delta = raw_ts;
                }
                1 | 2 => {
                    st.delta = raw_ts;
                    st.timestamp = st.timestamp.wrapping_add(raw_ts);
                }
                _ => {
                    if extended {
                        st.delta = raw_ts;
                    }
                    st.timestamp = st.timestamp.wrapping_add(st.delta);
                }
            }
            st.length = length;
            st.type_id = type_id;
            st.stream_id = stream_id;
            st.extended = extended;
            st.ext_value = ext_value;
            st.has_header = true;
            st.partial.clear();
            st.partial.reserve(length as usize);
            st.in_progress = true;
        }
        st.partial.extend_from_slice(&self.buf[pos..pos + take]);
        self.buf.advance(pos + take);

        if st.partial.len() >= st.length as usize {
            st.in_progress = false;
            let payload = st.partial.split().freeze();
            return Ok(ChunkResult::Complete(Message {
                csid,
                timestamp: st.timestamp,
                type_id: st.type_id,
                stream_id: st.stream_id,
                payload,
            }));
        }
        Ok(ChunkResult::Partial)
    }
}

enum ChunkResult {
    NeedMore,
    Partial,
    Complete(Message),
}

fn u24(b: &[u8]) -> u32 {
    (b[0] as u32) << 16 | (b[1] as u32) << 8 | b[2] as u32
}

fn be32(b: &[u8]) -> u32 {
    u32::from_be_bytes([b[0], b[1], b[2], b[3]])
}

/// Serializes messages into chunks.
pub struct ChunkEncoder {
    chunk_size: usize,
}

impl Default for ChunkEncoder {
    fn default() -> Self {
        Self::new()
    }
}

impl ChunkEncoder {
    pub fn new() -> Self {
        Self {
            chunk_size: DEFAULT_CHUNK_SIZE,
        }
    }

    pub fn chunk_size(&self) -> usize {
        self.chunk_size
    }

    /// Changes the outgoing chunk size. The caller must send a Set Chunk Size
    /// message (encoded with the old size) before any message using the new one.
    pub fn set_chunk_size(&mut self, size: usize) {
        self.chunk_size = size.clamp(1, MAX_CHUNK_SIZE);
    }

    pub fn write(
        &self,
        out: &mut BytesMut,
        csid: u32,
        timestamp: u32,
        type_id: u8,
        stream_id: u32,
        payload: &[u8],
    ) {
        let extended = timestamp >= EXTENDED;
        out.reserve(payload.len() + 18 + payload.len() / self.chunk_size.max(1) * 8);
        put_basic_header(out, 0, csid);
        let ts_field = if extended { EXTENDED } else { timestamp };
        put_u24(out, ts_field);
        put_u24(out, payload.len() as u32);
        out.put_u8(type_id);
        out.put_u32_le(stream_id);
        if extended {
            out.put_u32(timestamp);
        }
        let mut chunks = payload.chunks(self.chunk_size);
        if let Some(first) = chunks.next() {
            out.put_slice(first);
        }
        for c in chunks {
            put_basic_header(out, 3, csid);
            if extended {
                out.put_u32(timestamp);
            }
            out.put_slice(c);
        }
    }

    pub fn write_message(&self, out: &mut BytesMut, m: &Message) {
        self.write(out, m.csid, m.timestamp, m.type_id, m.stream_id, &m.payload);
    }
}

fn put_u24(out: &mut BytesMut, v: u32) {
    out.put_u8((v >> 16) as u8);
    out.put_u8((v >> 8) as u8);
    out.put_u8(v as u8);
}

fn put_basic_header(out: &mut BytesMut, fmt: u8, csid: u32) {
    if (2..64).contains(&csid) {
        out.put_u8(fmt << 6 | csid as u8);
    } else if (64..320).contains(&csid) {
        out.put_u8(fmt << 6);
        out.put_u8((csid - 64) as u8);
    } else {
        let v = csid.clamp(64, 65599) - 64;
        out.put_u8(fmt << 6 | 1);
        out.put_u8(v as u8);
        out.put_u8((v >> 8) as u8);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use proptest::prelude::*;

    fn decode_all(dec: &mut ChunkDecoder) -> Vec<Message> {
        let mut v = Vec::new();
        while let Some(m) = dec.next_message().unwrap() {
            v.push(m);
        }
        v
    }

    #[test]
    fn round_trip_multi_chunk_extended_timestamp() {
        let mut enc = ChunkEncoder::new();
        enc.set_chunk_size(100);
        let mut out = BytesMut::new();
        let payload: Vec<u8> = (0..1000u32).map(|i| i as u8).collect();
        enc.write(&mut out, 6, 0x0100_0000, 9, 1, &payload);
        enc.write(&mut out, 4, 5, 8, 1, b"abc");

        let mut dec = ChunkDecoder::new();
        dec.set_chunk_size(100).unwrap();
        // Feed one byte at a time to exercise every NeedMore path.
        let mut got = Vec::new();
        for byte in out.iter() {
            dec.push(&[*byte]);
            while let Some(m) = dec.next_message().unwrap() {
                got.push(m);
            }
        }
        assert_eq!(got.len(), 2);
        assert_eq!(got[0].timestamp, 0x0100_0000);
        assert_eq!(&got[0].payload[..], &payload[..]);
        assert_eq!(got[1].timestamp, 5);
        assert_eq!(&got[1].payload[..], b"abc");
    }

    #[test]
    fn continuation_without_repeated_extended_timestamp() {
        // Some encoders omit the extended timestamp on type-3 continuation chunks.
        let mut data = vec![0x06]; // fmt 0, csid 6
        data.extend_from_slice(&[0xff, 0xff, 0xff, 0, 0, 200, 9, 1, 0, 0, 0]);
        data.extend_from_slice(&0x0200_0000u32.to_be_bytes());
        data.extend_from_slice(&[1u8; 128]);
        data.push(0xc6); // fmt 3 continuation without extended field
        data.extend_from_slice(&[2u8; 72]);
        let mut dec = ChunkDecoder::new();
        dec.push(&data);
        let m = dec.next_message().unwrap().unwrap();
        assert_eq!(m.timestamp, 0x0200_0000);
        assert_eq!(m.payload.len(), 200);
        assert_eq!(m.payload[127], 1);
        assert_eq!(m.payload[128], 2);
    }

    #[test]
    fn compressed_headers_accumulate_deltas() {
        let mut data = vec![0x04, 0, 0, 10, 0, 0, 2, 8, 1, 0, 0, 0, 0xaa, 0xbb]; // fmt0 ts=10
        data.extend_from_slice(&[0x44, 0, 0, 20, 0, 0, 1, 8, 0xcc]); // fmt1 delta=20
        data.extend_from_slice(&[0x84, 0, 0, 5, 0xdd]); // fmt2 delta=5
        data.extend_from_slice(&[0xc4, 0xee]); // fmt3 new message, delta=5 again
        let mut dec = ChunkDecoder::new();
        dec.push(&data);
        let ms = decode_all(&mut dec);
        let ts: Vec<u32> = ms.iter().map(|m| m.timestamp).collect();
        assert_eq!(ts, vec![10, 30, 35, 40]);
        assert_eq!(&ms[1].payload[..], &[0xcc]);
        assert_eq!(&ms[3].payload[..], &[0xee]);
    }

    #[test]
    fn type3_after_type0_uses_type0_timestamp_as_delta() {
        let mut data = vec![0x04, 0, 0, 10, 0, 0, 1, 8, 1, 0, 0, 0, 0xaa];
        data.extend_from_slice(&[0xc4, 0xbb]);
        let mut dec = ChunkDecoder::new();
        dec.push(&data);
        let ms = decode_all(&mut dec);
        assert_eq!(ms[1].timestamp, 20);
    }

    #[test]
    fn two_and_three_byte_basic_headers() {
        let enc = ChunkEncoder::new();
        let mut out = BytesMut::new();
        enc.write(&mut out, 70, 1, 20, 0, b"x");
        enc.write(&mut out, 400, 2, 20, 0, b"y");
        let mut dec = ChunkDecoder::new();
        dec.push(&out);
        let ms = decode_all(&mut dec);
        assert_eq!((ms[0].csid, ms[1].csid), (70, 400));
    }

    #[test]
    fn missing_header_is_an_error() {
        let mut dec = ChunkDecoder::new();
        dec.push(&[0xc5, 0x00]);
        assert_eq!(dec.next_message(), Err(ChunkError::MissingHeader(5)));
    }

    #[test]
    fn abort_drops_partial_message() {
        let enc = ChunkEncoder::new();
        let mut out = BytesMut::new();
        enc.write(&mut out, 6, 0, 9, 1, &[7u8; 300]);
        let mut dec = ChunkDecoder::new();
        dec.push(&out[..140]);
        assert!(dec.next_message().unwrap().is_none());
        dec.abort(6);
        let mut out2 = BytesMut::new();
        enc.write(&mut out2, 6, 1, 9, 1, &[8u8; 3]);
        dec.push(&out2);
        let m = dec.next_message().unwrap().unwrap();
        assert_eq!(&m.payload[..], &[8, 8, 8]);
    }

    proptest! {
        #[test]
        fn interleaved_streams_round_trip(
            msgs in prop::collection::vec(
                (2u32..10, any::<u32>(), 1u8..30, prop::collection::vec(any::<u8>(), 0..600)),
                1..20,
            ),
            chunk_size in 1usize..700,
            split in 1usize..64,
        ) {
            let mut enc = ChunkEncoder::new();
            enc.set_chunk_size(chunk_size);
            let mut out = BytesMut::new();
            for (csid, ts, ty, p) in &msgs {
                enc.write(&mut out, *csid, *ts, *ty, 1, p);
            }
            let mut dec = ChunkDecoder::new();
            dec.set_chunk_size(chunk_size as u32).unwrap();
            let mut got = Vec::new();
            for piece in out.chunks(split) {
                dec.push(piece);
                while let Some(m) = dec.next_message().unwrap() {
                    got.push(m);
                }
            }
            prop_assert_eq!(got.len(), msgs.len());
            for (m, (csid, ts, ty, p)) in got.iter().zip(msgs.iter()) {
                prop_assert_eq!(m.csid, *csid);
                prop_assert_eq!(m.timestamp, *ts);
                prop_assert_eq!(m.type_id, *ty);
                prop_assert_eq!(&m.payload[..], &p[..]);
            }
        }
    }
}
