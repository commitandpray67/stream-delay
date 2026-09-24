//! RTMP chunk stream codec.
//!
//! The decoder implements every header format (0–3), 1–3 byte basic headers,
//! extended timestamps (including the repeated field on type-3 continuation
//! chunks) and runtime chunk-size changes. The encoder always writes a type-0
//! header for the first chunk of a message, which every RTMP peer accepts.

use std::collections::{HashMap, VecDeque};
use std::sync::{Arc, Mutex, PoisonError};

use bytes::{Buf, BufMut, Bytes, BytesMut};
use thiserror::Error;

pub const DEFAULT_CHUNK_SIZE: usize = 128;
/// Largest chunk size accepted from a peer (the spec allows up to 2^31-1; we cap it).
pub const MAX_CHUNK_SIZE: usize = 1 << 24;
/// Chunk streams a peer may use at once. Encoders use a handful; the limit stops a
/// hostile peer from spreading partial messages over thousands of streams.
pub const MAX_CHUNK_STREAMS: usize = 64;
/// Bytes held in unfinished messages across all chunk streams.
pub const MAX_PENDING_BYTES: usize = 64 * 1024 * 1024;
/// Up-front allocation for a new message; larger messages grow as data arrives, so a
/// header alone cannot make us allocate its declared length.
const MAX_RESERVE: usize = 256 * 1024;
/// Complete messages up to `ARENA_MAX_MESSAGE` bytes are copied into shared blocks
/// of `ARENA_BLOCK` bytes. A delay buffer keeps messages for minutes; with one
/// allocation per message (a few hundred bytes to hundreds of KB, freed on another
/// thread) the heap fragmented until the process used 3–4 times the buffered data
/// in soak tests, with glibc and mimalloc alike. Equal blocks freed in arrival
/// order are reused cleanly.
///
/// Full blocks are also reused once no message uses them any more, rather than
/// freed and allocated again: with glibc, freeing a 1 MiB block raises the size
/// from which it maps memory straight from the OS, and later blocks then came
/// from its general heap, where the process held 20-35 MB more than the
/// buffered data in steady-state tests.
const ARENA_BLOCK: usize = 1024 * 1024;
const ARENA_MAX_MESSAGE: usize = ARENA_BLOCK / 2;
/// Unused blocks kept for reuse; any beyond this go back to the allocator.
const MAX_SPARE_BLOCKS: usize = 4;
/// With the pool at its limit, bytes of messages allocated one by one before
/// looking for a free block again (looking scans every block in use).
const RETRY_AFTER: usize = ARENA_BLOCK / 4;

/// The blocks complete messages are copied into (see `ARENA_BLOCK`), shared by
/// every decoder feeding the same buffer: one per ingest connection.
///
/// A block stays allocated for as long as any message in it is kept, so one
/// small message kept for minutes holds a whole block, whatever became of the
/// messages around it. The pool bounds how many blocks can be in use at once,
/// across connections (closed ones included, since what they sent may still be
/// buffered). Past the limit, messages are allocated one by one, which costs
/// about their own size, until blocks free up.
#[derive(Clone)]
pub struct ArenaPool(Arc<Mutex<Pool>>);

struct Pool {
    /// Full blocks, oldest first (the unused rest of each), kept to be reused
    /// once no message uses them any more.
    retired: VecDeque<BytesMut>,
    /// Blocks decoders are filling.
    active: usize,
    /// Blocks allocated so far (the rest were reused).
    allocated: usize,
    max_blocks: usize,
}

impl ArenaPool {
    /// A pool whose blocks in use hold at most about `max_bytes`.
    pub fn new(max_bytes: usize) -> Self {
        Self(Arc::new(Mutex::new(Pool {
            retired: VecDeque::new(),
            active: 0,
            allocated: 0,
            max_blocks: (max_bytes / ARENA_BLOCK).max(1),
        })))
    }

    /// A pool without a limit (a decoder's own, unless given a shared one).
    pub fn unlimited() -> Self {
        Self::new(usize::MAX)
    }

    fn lock(&self) -> std::sync::MutexGuard<'_, Pool> {
        self.0.lock().unwrap_or_else(PoisonError::into_inner)
    }

    /// Bytes held by blocks that messages may still be using.
    pub fn bytes_in_use(&self) -> usize {
        let mut p = self.lock();
        let free = p
            .retired
            .iter_mut()
            .map(|b| b.try_reclaim(ARENA_BLOCK))
            .filter(|&reclaimed| reclaimed)
            .count();
        (p.active + p.retired.len() - free) * ARENA_BLOCK
    }

    /// Blocks allocated so far.
    pub fn blocks_allocated(&self) -> usize {
        self.lock().allocated
    }

    /// Retires `full` (a decoder's block, or none) and returns a block to copy
    /// messages into: a retired one no message uses any more, or a new one while
    /// fewer than the limit are in use. `None` at the limit.
    fn next_block(&self, full: Option<BytesMut>) -> Option<BytesMut> {
        let mut p = self.lock();
        if let Some(full) = full {
            p.active -= 1;
            p.retired.push_back(full);
        }
        let mut reuse = None;
        let mut spare = 0;
        // `try_reclaim` succeeds only for a block this handle alone still refers to.
        p.retired.retain_mut(|b| {
            if !b.try_reclaim(ARENA_BLOCK) {
                return true;
            }
            if reuse.is_none() {
                reuse = Some(std::mem::take(b));
                return false;
            }
            spare += 1;
            spare <= MAX_SPARE_BLOCKS
        });
        let block = match reuse {
            Some(b) => b,
            // Every retired block is in use.
            None if p.active + p.retired.len() < p.max_blocks => {
                p.allocated += 1;
                BytesMut::with_capacity(ARENA_BLOCK)
            }
            None => return None,
        };
        p.active += 1;
        Some(block)
    }

    /// Takes back the block a decoder was filling when it went away.
    fn give_back(&self, block: BytesMut) {
        let mut p = self.lock();
        p.active -= 1;
        p.retired.push_back(block);
    }
}
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
    #[error("peer used more than {MAX_CHUNK_STREAMS} chunk streams")]
    TooManyStreams,
    #[error("peer has more than {MAX_PENDING_BYTES} bytes of unfinished messages")]
    TooMuchPending,
    #[error("message of type {type_id} is {length} bytes, over the limit of {max}")]
    MessageTooLarge {
        type_id: u8,
        length: u32,
        max: usize,
    },
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
    /// The message being received. Reused for the next message once its bytes have
    /// been copied out.
    partial: BytesMut,
    in_progress: bool,
    has_header: bool,
}

/// Incremental decoder: push received bytes, then pull complete messages.
pub struct ChunkDecoder {
    chunk_size: usize,
    streams: HashMap<u32, StreamState>,
    buf: BytesMut,
    /// Sum of `partial.len()` over all streams.
    pending: usize,
    /// Unused rest of the current block that complete messages are copied into
    /// (none until the first message, or while the pool is at its limit).
    arena: Option<BytesMut>,
    pool: ArenaPool,
    /// See `RETRY_AFTER`.
    retry_after: usize,
    /// Largest audio or video message accepted.
    max_media_len: usize,
    /// Largest message of any other type (commands, metadata, control).
    max_other_len: usize,
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
            pending: 0,
            arena: None,
            pool: ArenaPool::unlimited(),
            retry_after: 0,
            max_media_len: usize::MAX,
            max_other_len: usize::MAX,
        }
    }

    /// Limits the declared length of new messages: `media` for audio and video,
    /// `other` for every other type. A longer message is an error as soon as its
    /// header arrives. Unlimited by default (the format caps lengths at 16 MiB).
    pub fn set_max_message_len(&mut self, media: usize, other: usize) {
        self.max_media_len = media;
        self.max_other_len = other;
    }

    /// Copies messages into blocks from `pool`, shared with other decoders, from
    /// now on.
    pub fn set_arena_pool(&mut self, pool: ArenaPool) {
        if let Some(block) = self.arena.take() {
            self.pool.give_back(block);
        }
        self.pool = pool;
        self.retry_after = 0;
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
            self.pending -= s.partial.len();
            release(&mut s.partial);
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
        if !self.streams.contains_key(&csid) && self.streams.len() >= MAX_CHUNK_STREAMS {
            return Err(ChunkError::TooManyStreams);
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
        if !continuation {
            let max = match type_id {
                crate::message::AUDIO | crate::message::VIDEO => self.max_media_len,
                _ => self.max_other_len,
            };
            if length as usize > max {
                return Err(ChunkError::MessageTooLarge {
                    type_id,
                    length,
                    max,
                });
            }
        }
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
        // A new header discards the stream's unfinished message, if any.
        let dropped = if continuation { 0 } else { st.partial.len() };
        if self.pending - dropped + take > MAX_PENDING_BYTES {
            return Err(ChunkError::TooMuchPending);
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
            release(&mut st.partial);
            st.partial.reserve((length as usize).min(MAX_RESERVE));
            st.in_progress = true;
        }
        self.pending = self.pending - dropped + take;
        st.partial.extend_from_slice(&self.buf[pos..pos + take]);
        self.buf.advance(pos + take);

        if st.partial.len() >= st.length as usize {
            st.in_progress = false;
            self.pending -= st.partial.len();
            let len = st.partial.len();
            let arena = if len <= ARENA_MAX_MESSAGE {
                arena_with_room(&mut self.arena, &self.pool, &mut self.retry_after, len)
            } else {
                None
            };
            let payload = match arena {
                Some(arena) => {
                    arena.extend_from_slice(&st.partial);
                    release(&mut st.partial);
                    arena.split().freeze()
                }
                // The pool is at its limit: a copy of its own, exactly its size.
                None if len <= ARENA_MAX_MESSAGE => {
                    self.retry_after = self.retry_after.saturating_sub(len.max(1));
                    let payload = Bytes::copy_from_slice(&st.partial);
                    release(&mut st.partial);
                    payload
                }
                None => st.partial.split().freeze(),
            };
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

/// The current block if it has room for `len` more bytes, else a new one from
/// `pool` (the full one goes back to it). `None` while the pool is at its limit;
/// it is asked again once `RETRY_AFTER` bytes were allocated on their own.
fn arena_with_room<'a>(
    arena: &'a mut Option<BytesMut>,
    pool: &ArenaPool,
    retry_after: &mut usize,
    len: usize,
) -> Option<&'a mut BytesMut> {
    if !arena.as_ref().is_some_and(|a| a.capacity() >= len) {
        if *retry_after > 0 {
            return None;
        }
        *arena = pool.next_block(arena.take());
        if arena.is_none() {
            *retry_after = RETRY_AFTER;
        }
    }
    arena.as_mut()
}

impl Drop for ChunkDecoder {
    fn drop(&mut self) {
        // What this connection sent may still be buffered: its block stays
        // counted until no message uses it.
        if let Some(block) = self.arena.take() {
            self.pool.give_back(block);
        }
    }
}

/// Empties a message buffer for reuse. One larger than a block is freed instead:
/// kept, every chunk stream could hold on to a buffer the size of the largest
/// message it ever carried (or started to), which `pending` does not count.
fn release(partial: &mut BytesMut) {
    if partial.capacity() > ARENA_BLOCK {
        *partial = BytesMut::new();
    } else {
        partial.clear();
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

    #[test]
    fn too_many_chunk_streams_is_an_error() {
        let enc = ChunkEncoder::new();
        let mut out = BytesMut::new();
        for csid in 2..(2 + MAX_CHUNK_STREAMS as u32 + 1) {
            enc.write(&mut out, csid, 0, 9, 1, b"x");
        }
        let mut dec = ChunkDecoder::new();
        dec.push(&out);
        let mut result = Ok(None);
        for _ in 0..=MAX_CHUNK_STREAMS {
            result = dec.next_message();
            if result.is_err() {
                break;
            }
        }
        assert_eq!(result, Err(ChunkError::TooManyStreams));
    }

    fn decode_4k(out: &BytesMut) -> Vec<Message> {
        let mut dec = ChunkDecoder::new();
        dec.set_chunk_size(4096).unwrap();
        dec.push(out);
        decode_all(&mut dec)
    }

    #[test]
    fn small_messages_share_a_block() {
        let mut enc = ChunkEncoder::new();
        enc.set_chunk_size(4096);
        let mut out = BytesMut::new();
        for i in 0..6u8 {
            let csid = if i % 2 == 0 { 6 } else { 4 };
            enc.write(
                &mut out,
                csid,
                u32::from(i),
                9,
                1,
                &vec![i; 1000 + usize::from(i)],
            );
        }
        let msgs = decode_4k(&out);
        assert_eq!(msgs.len(), 6);
        for (i, m) in msgs.iter().enumerate() {
            assert_eq!(&m.payload[..], &vec![i as u8; 1000 + i][..]);
        }
        // Consecutive messages sit next to each other in one allocation.
        for w in msgs.windows(2) {
            let end = w[0].payload.as_ptr() as usize + w[0].payload.len();
            assert_eq!(end, w[1].payload.as_ptr() as usize);
        }
    }

    #[test]
    fn large_messages_and_full_blocks_stay_intact() {
        let mut enc = ChunkEncoder::new();
        enc.set_chunk_size(4096);
        let mut out = BytesMut::new();
        // Enough to fill several blocks, with some messages too large for them.
        let sizes: Vec<usize> = (0..40)
            .map(|i| {
                if i % 7 == 0 {
                    ARENA_MAX_MESSAGE + 1 + i
                } else {
                    90_000 + i * 97
                }
            })
            .collect();
        for (i, n) in sizes.iter().enumerate() {
            enc.write(&mut out, 6, i as u32, 9, 1, &vec![i as u8; *n]);
        }
        let msgs = decode_4k(&out);
        assert_eq!(msgs.len(), sizes.len());
        for (i, m) in msgs.iter().enumerate() {
            assert_eq!(m.payload.len(), sizes[i]);
            assert!(
                m.payload.iter().all(|b| *b == i as u8),
                "message {i} corrupted"
            );
        }
    }

    #[test]
    fn header_alone_does_not_allocate_declared_length() {
        // A 16 MB message header with no body must not reserve 16 MB.
        let mut dec = ChunkDecoder::new();
        dec.set_chunk_size(1).unwrap();
        dec.push(&[0x06, 0, 0, 0, 0xff, 0xff, 0xff, 9, 1, 0, 0, 0, 0xaa]);
        assert!(dec.next_message().unwrap().is_none());
        let st = dec.streams.get(&6).unwrap();
        assert!(st.partial.capacity() <= MAX_RESERVE);
    }

    #[test]
    fn pending_bytes_are_bounded() {
        // Many streams each holding an unfinished large message.
        // Each stream sends the first 8 MB chunk of a 16 MB message and stops.
        let mut dec = ChunkDecoder::new();
        dec.set_chunk_size(8 * 1024 * 1024).unwrap();
        let body = vec![0u8; 8 * 1024 * 1024];
        let mut err = None;
        for csid in 2u8..20 {
            let mut chunk = vec![csid, 0, 0, 0, 0xff, 0xff, 0xff, 9, 1, 0, 0, 0];
            chunk.extend_from_slice(&body);
            dec.push(&chunk);
            if let Err(e) = dec.next_message() {
                err = Some(e);
                break;
            }
        }
        assert_eq!(err, Some(ChunkError::TooMuchPending));
        assert!(dec.pending <= MAX_PENDING_BYTES);
    }

    #[test]
    fn oversized_messages_are_rejected_by_type() {
        let mut dec = ChunkDecoder::new();
        dec.set_max_message_len(1000, 100);
        // Headers alone are enough: the body is never waited for.
        dec.push(&[0x03, 0, 0, 0, 0, 0, 101, 20, 0, 0, 0, 0]);
        assert_eq!(
            dec.next_message(),
            Err(ChunkError::MessageTooLarge {
                type_id: 20,
                length: 101,
                max: 100
            })
        );
        let enc = ChunkEncoder::new();
        let mut out = BytesMut::new();
        enc.write(&mut out, 6, 0, 9, 1, &[1u8; 1000]);
        enc.write(&mut out, 4, 0, 8, 1, &[2u8; 1001]);
        let mut dec = ChunkDecoder::new();
        dec.set_max_message_len(1000, 100);
        dec.push(&out);
        assert_eq!(dec.next_message().unwrap().unwrap().payload.len(), 1000);
        assert!(matches!(
            dec.next_message(),
            Err(ChunkError::MessageTooLarge { type_id: 8, .. })
        ));
    }

    #[test]
    fn aborted_and_replaced_messages_free_their_buffers() {
        let cs = 1024 * 1024;
        let len = 16 * 1024 * 1024 - 1;
        // Most of a 16 MiB message on each of `streams` chunk streams.
        let partial = |csid: u8| {
            let mut data = vec![
                csid,
                0,
                0,
                0,
                (len >> 16) as u8,
                (len >> 8) as u8,
                len as u8,
            ];
            data.extend_from_slice(&[9, 1, 0, 0, 0]);
            data.resize(data.len() + cs, 0);
            for _ in 0..14 {
                data.push(0xc0 | csid);
                data.resize(data.len() + cs, 0);
            }
            data
        };
        let held = |dec: &ChunkDecoder| -> usize {
            dec.streams.values().map(|s| s.partial.capacity()).sum()
        };
        let mut dec = ChunkDecoder::new();
        dec.set_chunk_size(cs as u32).unwrap();
        for csid in 2u8..10 {
            dec.push(&partial(csid));
            assert!(dec.next_message().unwrap().is_none());
            dec.abort(u32::from(csid));
        }
        assert_eq!(dec.pending, 0);
        assert!(
            held(&dec) <= 8 * MAX_RESERVE,
            "aborts kept {} bytes",
            held(&dec)
        );
        // A new message header discards the unfinished one the same way.
        for csid in 2u8..10 {
            dec.push(&partial(csid));
            assert!(dec.next_message().unwrap().is_none());
            dec.push(&[csid, 0, 0, 0, 0, 0, 1, 8, 1, 0, 0, 0, 0xaa]);
            assert_eq!(dec.next_message().unwrap().unwrap().payload.len(), 1);
        }
        assert!(
            held(&dec) <= 8 * MAX_RESERVE,
            "replaced messages kept {} bytes",
            held(&dec)
        );
    }

    #[test]
    fn blocks_are_reused_once_their_messages_are_gone() {
        let mut enc = ChunkEncoder::new();
        enc.set_chunk_size(64 * 1024);
        let mut dec = ChunkDecoder::new();
        dec.set_chunk_size(64 * 1024).unwrap();
        // Two 400 KB messages fill a block. Each is dropped soon after, as the
        // delay buffer does with old content.
        let mut kept = VecDeque::new();
        for i in 0..40u32 {
            let mut out = BytesMut::new();
            enc.write(&mut out, 6, i, 9, 1, &vec![i as u8; 400 * 1024]);
            dec.push(&out);
            let m = dec.next_message().unwrap().unwrap();
            assert!(
                m.payload.iter().all(|b| *b == i as u8),
                "message {i} corrupted"
            );
            kept.push_back(m);
            if kept.len() > 4 {
                kept.pop_front();
            }
        }
        // Three or four blocks hold the four messages kept at any time; 20 would
        // be allocated without reuse.
        assert!(
            dec.pool.blocks_allocated() <= 4,
            "{} blocks allocated",
            dec.pool.blocks_allocated()
        );
        assert!(dec.pool.lock().retired.len() <= 4 + MAX_SPARE_BLOCKS);
        // Messages still in use are never overwritten.
        for m in &kept {
            let first = m.payload[0];
            assert!(m.payload.iter().all(|b| *b == first));
        }
    }

    #[test]
    fn a_shared_pool_bounds_blocks_held_by_small_kept_messages() {
        // A tiny message kept, and two large ones dropped right away, filling a
        // block each time: without a limit, each tiny message would hold a block.
        let pool = ArenaPool::new(4 * ARENA_BLOCK);
        let mut kept = Vec::new();
        for connection in 0..2u32 {
            let mut enc = ChunkEncoder::new();
            enc.set_chunk_size(64 * 1024);
            let mut dec = ChunkDecoder::new();
            dec.set_chunk_size(64 * 1024).unwrap();
            dec.set_arena_pool(pool.clone());
            for i in 0..50u32 {
                let mut out = BytesMut::new();
                enc.write(&mut out, 4, i, 8, 1, &[0x2f, i as u8]);
                enc.write(&mut out, 6, i, 15, 1, &vec![1u8; 524_288]);
                enc.write(&mut out, 6, i, 15, 1, &vec![2u8; 524_286]);
                dec.push(&out);
                while let Some(m) = dec.next_message().unwrap() {
                    if m.type_id == 8 {
                        kept.push((connection, i, m));
                    }
                }
                assert!(
                    pool.bytes_in_use() <= 4 * ARENA_BLOCK,
                    "{} bytes of blocks in use",
                    pool.bytes_in_use()
                );
            }
            // The next connection's decoder shares the limit with this one, whose
            // blocks stay counted while its messages are kept.
        }
        assert_eq!(kept.len(), 100);
        for (_, i, m) in &kept {
            assert_eq!(&m.payload[..], &[0x2f, *i as u8]);
        }
        drop(kept);
        assert_eq!(
            pool.bytes_in_use(),
            0,
            "blocks free again once nothing is kept"
        );
    }

    #[test]
    fn pending_accounting_returns_to_zero() {
        let mut enc = ChunkEncoder::new();
        enc.set_chunk_size(100);
        let mut out = BytesMut::new();
        enc.write(&mut out, 6, 0, 9, 1, &[1u8; 1000]);
        enc.write(&mut out, 4, 0, 8, 1, &[2u8; 50]);
        let mut dec = ChunkDecoder::new();
        dec.set_chunk_size(100).unwrap();
        dec.push(&out[..350]);
        while dec.next_message().unwrap().is_some() {}
        assert!(dec.pending > 0);
        dec.push(&out[350..]);
        while dec.next_message().unwrap().is_some() {}
        assert_eq!(dec.pending, 0);
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
            prop_assert_eq!(dec.pending, 0);
            prop_assert_eq!(got.len(), msgs.len());
            for (m, (csid, ts, ty, p)) in got.iter().zip(msgs.iter()) {
                prop_assert_eq!(m.csid, *csid);
                prop_assert_eq!(m.timestamp, *ts);
                prop_assert_eq!(m.type_id, *ty);
                prop_assert_eq!(&m.payload[..], &p[..]);
            }
        }
    }

    proptest! {
        /// Arbitrary bytes never panic the decoder or break its accounting.
        #[test]
        fn arbitrary_input_never_panics(
            data in prop::collection::vec(any::<u8>(), 0..4096),
            chunk_size in 1u32..300,
        ) {
            let mut dec = ChunkDecoder::new();
            dec.set_chunk_size(chunk_size).unwrap();
            dec.push(&data);
            for _ in 0..10_000 {
                match dec.next_message() {
                    Ok(Some(_)) => {}
                    Ok(None) | Err(_) => break,
                }
            }
            let held: usize = dec.streams.values().map(|s| s.partial.len()).sum();
            prop_assert_eq!(held, dec.pending);
        }
    }
}
