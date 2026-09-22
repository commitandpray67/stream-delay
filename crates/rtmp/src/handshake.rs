//! The simple (non-digest) RTMP handshake, as used by OBS/librtmp and accepted by
//! Twitch, YouTube and common servers.

use bytes::{BufMut, Bytes, BytesMut};
use rand::RngCore;
use thiserror::Error;

pub const RTMP_VERSION: u8 = 3;
pub const HANDSHAKE_SIZE: usize = 1536;

#[derive(Debug, Error, PartialEq, Eq)]
pub enum HandshakeError {
    #[error("unsupported RTMP version {0} (encrypted RTMPE is not supported)")]
    UnsupportedVersion(u8),
}

/// Result of feeding bytes into a handshake.
#[derive(Debug, PartialEq, Eq)]
pub enum Progress {
    NeedMore,
    /// Handshake finished. Any bytes received after it belong to the chunk stream.
    Done(Bytes),
}

fn random_packet() -> [u8; HANDSHAKE_SIZE] {
    let mut p = [0u8; HANDSHAKE_SIZE];
    // time (4 bytes) and zero (4 bytes) stay 0, the rest is random.
    rand::rng().fill_bytes(&mut p[8..]);
    p
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum ServerState {
    WaitC0C1,
    WaitC2,
    Done,
}

/// Server side: receives C0+C1, answers S0+S1+S2, then waits for C2.
pub struct ServerHandshake {
    state: ServerState,
    buf: BytesMut,
}

impl Default for ServerHandshake {
    fn default() -> Self {
        Self::new()
    }
}

impl ServerHandshake {
    pub fn new() -> Self {
        Self {
            state: ServerState::WaitC0C1,
            buf: BytesMut::new(),
        }
    }

    pub fn feed(&mut self, input: &[u8], out: &mut BytesMut) -> Result<Progress, HandshakeError> {
        self.buf.extend_from_slice(input);
        loop {
            match self.state {
                ServerState::WaitC0C1 => {
                    if self.buf.len() < 1 + HANDSHAKE_SIZE {
                        return Ok(Progress::NeedMore);
                    }
                    let c0c1 = self.buf.split_to(1 + HANDSHAKE_SIZE);
                    if c0c1[0] != RTMP_VERSION {
                        return Err(HandshakeError::UnsupportedVersion(c0c1[0]));
                    }
                    out.put_u8(RTMP_VERSION);
                    out.put_slice(&random_packet());
                    // S2 echoes C1.
                    out.put_slice(&c0c1[1..]);
                    self.state = ServerState::WaitC2;
                }
                ServerState::WaitC2 => {
                    if self.buf.len() < HANDSHAKE_SIZE {
                        return Ok(Progress::NeedMore);
                    }
                    let _c2 = self.buf.split_to(HANDSHAKE_SIZE);
                    self.state = ServerState::Done;
                }
                ServerState::Done => return Ok(Progress::Done(self.buf.split().freeze())),
            }
        }
    }
}

/// Client side: sends C0+C1 on `start`, then answers S1 with C2.
pub struct ClientHandshake {
    done: bool,
    buf: BytesMut,
}

impl Default for ClientHandshake {
    fn default() -> Self {
        Self::new()
    }
}

impl ClientHandshake {
    pub fn new() -> Self {
        Self {
            done: false,
            buf: BytesMut::new(),
        }
    }

    pub fn start(&mut self, out: &mut BytesMut) {
        out.put_u8(RTMP_VERSION);
        out.put_slice(&random_packet());
    }

    pub fn feed(&mut self, input: &[u8], out: &mut BytesMut) -> Result<Progress, HandshakeError> {
        self.buf.extend_from_slice(input);
        if !self.done {
            if self.buf.len() < 1 + 2 * HANDSHAKE_SIZE {
                return Ok(Progress::NeedMore);
            }
            let s = self.buf.split_to(1 + 2 * HANDSHAKE_SIZE);
            if s[0] != RTMP_VERSION {
                return Err(HandshakeError::UnsupportedVersion(s[0]));
            }
            // C2 echoes S1.
            out.put_slice(&s[1..1 + HANDSHAKE_SIZE]);
            self.done = true;
        }
        Ok(Progress::Done(self.buf.split().freeze()))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn client_and_server_complete() {
        let mut c = ClientHandshake::new();
        let mut s = ServerHandshake::new();
        let mut c_out = BytesMut::new();
        let mut s_out = BytesMut::new();
        c.start(&mut c_out);
        assert_eq!(
            s.feed(&c_out.split(), &mut s_out).unwrap(),
            Progress::NeedMore
        );
        // Server response plus some trailing application bytes.
        let mut to_client = s_out.split();
        to_client.extend_from_slice(b"xyz");
        let Progress::Done(rest) = c.feed(&to_client, &mut c_out).unwrap() else {
            panic!("client not done");
        };
        assert_eq!(&rest[..], b"xyz");
        let mut c2 = c_out.split();
        c2.extend_from_slice(b"connect");
        let Progress::Done(rest) = s.feed(&c2, &mut s_out).unwrap() else {
            panic!("server not done");
        };
        assert_eq!(&rest[..], b"connect");
    }

    #[test]
    fn rejects_rtmpe() {
        let mut s = ServerHandshake::new();
        let mut data = vec![6u8];
        data.extend_from_slice(&[0u8; HANDSHAKE_SIZE]);
        assert_eq!(
            s.feed(&data, &mut BytesMut::new()),
            Err(HandshakeError::UnsupportedVersion(6))
        );
    }
}
