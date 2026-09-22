//! RTMP message type ids and protocol control message helpers.

use bytes::{BufMut, BytesMut};

use crate::amf0::{self, Amf0Value};
use crate::chunk::ChunkEncoder;

pub const SET_CHUNK_SIZE: u8 = 1;
pub const ABORT: u8 = 2;
pub const ACKNOWLEDGEMENT: u8 = 3;
pub const USER_CONTROL: u8 = 4;
pub const WINDOW_ACK_SIZE: u8 = 5;
pub const SET_PEER_BANDWIDTH: u8 = 6;
pub const AUDIO: u8 = 8;
pub const VIDEO: u8 = 9;
pub const DATA_AMF3: u8 = 15;
pub const COMMAND_AMF3: u8 = 17;
pub const DATA_AMF0: u8 = 18;
pub const COMMAND_AMF0: u8 = 20;

pub const UC_STREAM_BEGIN: u16 = 0;
pub const UC_STREAM_EOF: u16 = 1;
pub const UC_PING_REQUEST: u16 = 6;
pub const UC_PING_RESPONSE: u16 = 7;

/// Chunk stream ids used for outgoing messages.
pub const CSID_CONTROL: u32 = 2;
pub const CSID_COMMAND: u32 = 3;
pub const CSID_AUDIO: u32 = 4;
pub const CSID_DATA: u32 = 5;
pub const CSID_VIDEO: u32 = 6;
pub const CSID_STREAM_COMMAND: u32 = 8;

/// Writes a protocol control message with a 4-byte big-endian value.
pub fn write_u32_control(enc: &ChunkEncoder, out: &mut BytesMut, type_id: u8, value: u32) {
    enc.write(out, CSID_CONTROL, 0, type_id, 0, &value.to_be_bytes());
}

pub fn write_set_peer_bandwidth(enc: &ChunkEncoder, out: &mut BytesMut, size: u32, limit: u8) {
    let mut p = [0u8; 5];
    p[..4].copy_from_slice(&size.to_be_bytes());
    p[4] = limit;
    enc.write(out, CSID_CONTROL, 0, SET_PEER_BANDWIDTH, 0, &p);
}

pub fn write_user_control(enc: &ChunkEncoder, out: &mut BytesMut, event: u16, data: u32) {
    let mut p = [0u8; 6];
    p[..2].copy_from_slice(&event.to_be_bytes());
    p[2..].copy_from_slice(&data.to_be_bytes());
    enc.write(out, CSID_CONTROL, 0, USER_CONTROL, 0, &p);
}

/// Writes an AMF0 command message.
pub fn write_command(
    enc: &ChunkEncoder,
    out: &mut BytesMut,
    csid: u32,
    stream_id: u32,
    values: &[Amf0Value],
) {
    let payload = amf0::encode_all(values);
    enc.write(out, csid, 0, COMMAND_AMF0, stream_id, &payload);
}

/// Reads a big-endian u32 from the start of a control message payload.
pub fn read_u32(payload: &[u8]) -> Option<u32> {
    let b = payload.get(..4)?;
    Some(u32::from_be_bytes([b[0], b[1], b[2], b[3]]))
}

/// Parses a User Control message into (event type, first 4 data bytes).
pub fn read_user_control(payload: &[u8]) -> Option<(u16, u32)> {
    let ev = u16::from_be_bytes([*payload.first()?, *payload.get(1)?]);
    let data = read_u32(payload.get(2..)?).unwrap_or(0);
    Some((ev, data))
}

/// Builds an `onStatus` info object.
pub fn status_object(level: &str, code: &str, description: &str) -> Amf0Value {
    Amf0Value::object([
        ("level", Amf0Value::string(level)),
        ("code", Amf0Value::string(code)),
        ("description", Amf0Value::string(description)),
    ])
}

/// Ensures a data message payload starts with `@setDataFrame`, as ingest servers expect
/// from publishers. Returns `None` if the payload is not valid AMF0.
pub fn with_set_data_frame(payload: &[u8]) -> Option<BytesMut> {
    let (first, _) = amf0::decode_one(payload).ok()?;
    if first.as_str() == Some("@setDataFrame") {
        return Some(BytesMut::from(payload));
    }
    let mut out = BytesMut::with_capacity(payload.len() + 16);
    amf0::encode(&Amf0Value::string("@setDataFrame"), &mut out);
    out.put_slice(payload);
    Some(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn set_data_frame_is_prepended_once() {
        let meta = amf0::encode_all(&[
            Amf0Value::string("onMetaData"),
            Amf0Value::EcmaArray(vec![]),
        ]);
        let once = with_set_data_frame(&meta).unwrap();
        let twice = with_set_data_frame(&once).unwrap();
        assert_eq!(once, twice);
        let vals = amf0::decode_all(&once).unwrap();
        assert_eq!(vals[0].as_str(), Some("@setDataFrame"));
        assert_eq!(vals[1].as_str(), Some("onMetaData"));
    }

    #[test]
    fn user_control_parse() {
        assert_eq!(read_user_control(&[0, 6, 0, 0, 1, 0]), Some((6, 256)));
        assert_eq!(read_user_control(&[0]), None);
    }
}
