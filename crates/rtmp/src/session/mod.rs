//! Publish-oriented RTMP session state machines.
//!
//! [`ServerSession`] accepts a publisher (OBS). [`ClientSession`] publishes to an
//! ingest server (Twitch). Both are sans-IO: feed received bytes with `feed` and
//! send whatever `take_output` returns.

mod client;
mod server;

pub use client::{ClientConfig, ClientEvent, ClientSession};
pub use server::{ServerConfig, ServerEvent, ServerSession};

use bytes::BytesMut;
use thiserror::Error;

use crate::amf0::Amf0Error;
use crate::chunk::{ChunkDecoder, ChunkEncoder, ChunkError, Message};
use crate::message::{self, *};

/// Largest message a publisher may send before its publish is accepted. The
/// commands of a normal handshake (connect, releaseStream, FCPublish,
/// createStream, publish) are a few hundred bytes each; this keeps an
/// unauthenticated peer from making us buffer and decode megabytes.
pub const MAX_PRE_PUBLISH_MESSAGE: usize = 64 * 1024;
/// Largest non-media message (commands, metadata, control) at any time. Decoding
/// AMF0 can take many times its size in memory, so it is bounded separately from
/// audio and video.
pub const MAX_NON_MEDIA_MESSAGE: usize = 1024 * 1024;

/// Kind of media carried by an audio/video message.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum MediaKind {
    Audio,
    Video,
}

#[derive(Debug, Error)]
pub enum SessionError {
    #[error(transparent)]
    Chunk(#[from] ChunkError),
    #[error("malformed command: {0}")]
    Amf0(#[from] Amf0Error),
    #[error("protocol error: {0}")]
    Protocol(String),
}

/// Shared low-level state: chunk codec, acknowledgements and pings.
pub(crate) struct Link {
    pub decoder: ChunkDecoder,
    pub encoder: ChunkEncoder,
    pub out: BytesMut,
    bytes_in: u64,
    last_ack: u64,
    /// Window after which we acknowledge received bytes (set by the peer).
    ack_window: u32,
}

impl Link {
    pub fn new(default_ack_window: u32) -> Self {
        Self {
            decoder: ChunkDecoder::new(),
            encoder: ChunkEncoder::new(),
            out: BytesMut::new(),
            bytes_in: 0,
            last_ack: 0,
            ack_window: default_ack_window,
        }
    }

    /// Accounts received bytes and sends an Acknowledgement when the window is crossed.
    pub fn received(&mut self, n: usize) {
        self.bytes_in += n as u64;
        if self.ack_window > 0 && self.bytes_in - self.last_ack >= self.ack_window as u64 {
            self.last_ack = self.bytes_in;
            // The sequence number is the (wrapping) total byte count.
            write_u32_control(
                &self.encoder,
                &mut self.out,
                ACKNOWLEDGEMENT,
                self.bytes_in as u32,
            );
        }
    }

    /// Changes our outgoing chunk size, announcing it to the peer first.
    pub fn set_out_chunk_size(&mut self, size: u32) {
        write_u32_control(&self.encoder, &mut self.out, SET_CHUNK_SIZE, size);
        self.encoder.set_chunk_size(size as usize);
    }

    /// Handles protocol control messages. Returns `true` if the message was consumed.
    pub fn handle_control(&mut self, m: &Message) -> Result<bool, SessionError> {
        match m.type_id {
            SET_CHUNK_SIZE => {
                let v = message::read_u32(&m.payload)
                    .ok_or_else(|| SessionError::Protocol("short Set Chunk Size".into()))?;
                self.decoder.set_chunk_size(v & 0x7fff_ffff)?;
            }
            ABORT => {
                if let Some(csid) = message::read_u32(&m.payload) {
                    self.decoder.abort(csid);
                }
            }
            ACKNOWLEDGEMENT => {}
            WINDOW_ACK_SIZE => {
                if let Some(v) = message::read_u32(&m.payload) {
                    self.ack_window = v;
                }
            }
            SET_PEER_BANDWIDTH => {
                // Answer with our window, as librtmp does.
                if let Some(v) = message::read_u32(&m.payload) {
                    write_u32_control(&self.encoder, &mut self.out, WINDOW_ACK_SIZE, v);
                }
            }
            USER_CONTROL => {
                if let Some((UC_PING_REQUEST, ts)) = message::read_user_control(&m.payload) {
                    write_user_control(&self.encoder, &mut self.out, UC_PING_RESPONSE, ts);
                }
            }
            _ => return Ok(false),
        }
        Ok(true)
    }

    pub fn take_output(&mut self) -> bytes::Bytes {
        self.out.split().freeze()
    }
}

#[cfg(test)]
mod tests;
