//! Sans-IO implementation of the parts of RTMP that a publishing relay needs.
//!
//! Nothing in this crate touches sockets. Callers feed received bytes in and take
//! bytes to send out, which keeps every state machine unit-testable.

pub mod amf0;
pub mod chunk;
pub mod handshake;
pub mod message;
pub mod session;
pub mod ts;
pub mod url;

pub use chunk::{ChunkDecoder, ChunkEncoder, Message};
pub use session::{ClientEvent, ClientSession, MediaKind, ServerEvent, ServerSession};
pub use url::RtmpUrl;
