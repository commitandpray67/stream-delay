//! AMF0 encoding and decoding (the serialization format of RTMP commands and metadata).

use bytes::{BufMut, BytesMut};
use thiserror::Error;

const MAX_DEPTH: usize = 64;

/// A decoded AMF0 value. Object properties keep their wire order.
#[derive(Debug, Clone, PartialEq)]
pub enum Amf0Value {
    Number(f64),
    Boolean(bool),
    String(String),
    Object(Vec<(String, Amf0Value)>),
    Null,
    Undefined,
    EcmaArray(Vec<(String, Amf0Value)>),
    StrictArray(Vec<Amf0Value>),
    Date {
        millis: f64,
        tz: i16,
    },
    LongString(String),
    TypedObject {
        class: String,
        props: Vec<(String, Amf0Value)>,
    },
}

#[derive(Debug, Error, PartialEq, Eq)]
pub enum Amf0Error {
    #[error("unexpected end of AMF0 data")]
    Eof,
    #[error("unsupported AMF0 marker 0x{0:02x}")]
    UnsupportedMarker(u8),
    #[error("AMF0 nesting too deep")]
    TooDeep,
}

impl Amf0Value {
    pub fn as_str(&self) -> Option<&str> {
        match self {
            Amf0Value::String(s) | Amf0Value::LongString(s) => Some(s),
            _ => None,
        }
    }

    pub fn as_number(&self) -> Option<f64> {
        match self {
            Amf0Value::Number(n) => Some(*n),
            _ => None,
        }
    }

    /// Properties of an object, ECMA array or typed object.
    pub fn props(&self) -> Option<&[(String, Amf0Value)]> {
        match self {
            Amf0Value::Object(p) | Amf0Value::EcmaArray(p) => Some(p),
            Amf0Value::TypedObject { props, .. } => Some(props),
            _ => None,
        }
    }

    /// Looks up a property by name on object-like values.
    pub fn get(&self, key: &str) -> Option<&Amf0Value> {
        self.props()?.iter().find(|(k, _)| k == key).map(|(_, v)| v)
    }

    pub fn string(s: impl Into<String>) -> Self {
        Amf0Value::String(s.into())
    }

    pub fn object<K: Into<String>>(props: impl IntoIterator<Item = (K, Amf0Value)>) -> Self {
        Amf0Value::Object(props.into_iter().map(|(k, v)| (k.into(), v)).collect())
    }
}

/// Decodes every value in `data`.
pub fn decode_all(data: &[u8]) -> Result<Vec<Amf0Value>, Amf0Error> {
    let mut r = Reader { data, pos: 0 };
    let mut out = Vec::new();
    while r.pos < data.len() {
        out.push(r.value(0)?);
    }
    Ok(out)
}

/// Decodes the first value and returns it with the number of bytes consumed.
pub fn decode_one(data: &[u8]) -> Result<(Amf0Value, usize), Amf0Error> {
    let mut r = Reader { data, pos: 0 };
    let v = r.value(0)?;
    Ok((v, r.pos))
}

struct Reader<'a> {
    data: &'a [u8],
    pos: usize,
}

impl Reader<'_> {
    fn take(&mut self, n: usize) -> Result<&[u8], Amf0Error> {
        let end = self.pos.checked_add(n).ok_or(Amf0Error::Eof)?;
        let s = self.data.get(self.pos..end).ok_or(Amf0Error::Eof)?;
        self.pos = end;
        Ok(s)
    }

    fn u8(&mut self) -> Result<u8, Amf0Error> {
        Ok(self.take(1)?[0])
    }

    fn u16(&mut self) -> Result<u16, Amf0Error> {
        let b = self.take(2)?;
        Ok(u16::from_be_bytes([b[0], b[1]]))
    }

    fn u32(&mut self) -> Result<u32, Amf0Error> {
        let b = self.take(4)?;
        Ok(u32::from_be_bytes([b[0], b[1], b[2], b[3]]))
    }

    fn f64(&mut self) -> Result<f64, Amf0Error> {
        let b = self.take(8)?;
        let mut a = [0u8; 8];
        a.copy_from_slice(b);
        Ok(f64::from_be_bytes(a))
    }

    fn short_string(&mut self) -> Result<String, Amf0Error> {
        let n = self.u16()? as usize;
        Ok(String::from_utf8_lossy(self.take(n)?).into_owned())
    }

    fn long_string(&mut self) -> Result<String, Amf0Error> {
        let n = self.u32()? as usize;
        Ok(String::from_utf8_lossy(self.take(n)?).into_owned())
    }

    fn props(&mut self, depth: usize) -> Result<Vec<(String, Amf0Value)>, Amf0Error> {
        let mut props = Vec::new();
        loop {
            let key = self.short_string()?;
            if key.is_empty() {
                // Some encoders omit the end marker at the very end of the payload.
                if self.pos == self.data.len() {
                    return Ok(props);
                }
                if self.data[self.pos] == 0x09 {
                    self.pos += 1;
                    return Ok(props);
                }
            }
            let v = self.value(depth + 1)?;
            props.push((key, v));
        }
    }

    fn value(&mut self, depth: usize) -> Result<Amf0Value, Amf0Error> {
        if depth > MAX_DEPTH {
            return Err(Amf0Error::TooDeep);
        }
        let marker = self.u8()?;
        Ok(match marker {
            0x00 => Amf0Value::Number(self.f64()?),
            0x01 => Amf0Value::Boolean(self.u8()? != 0),
            0x02 => Amf0Value::String(self.short_string()?),
            0x03 => Amf0Value::Object(self.props(depth)?),
            0x05 => Amf0Value::Null,
            0x06 => Amf0Value::Undefined,
            0x08 => {
                let _count = self.u32()?;
                Amf0Value::EcmaArray(self.props(depth)?)
            }
            0x0a => {
                let n = self.u32()? as usize;
                // Every element needs at least one byte, which bounds the allocation.
                if n > self.data.len() - self.pos {
                    return Err(Amf0Error::Eof);
                }
                let mut items = Vec::with_capacity(n);
                for _ in 0..n {
                    items.push(self.value(depth + 1)?);
                }
                Amf0Value::StrictArray(items)
            }
            0x0b => {
                let millis = self.f64()?;
                let tz = self.u16()? as i16;
                Amf0Value::Date { millis, tz }
            }
            0x0c | 0x0f => Amf0Value::LongString(self.long_string()?),
            0x10 => {
                let class = self.short_string()?;
                Amf0Value::TypedObject {
                    class,
                    props: self.props(depth)?,
                }
            }
            other => return Err(Amf0Error::UnsupportedMarker(other)),
        })
    }
}

fn put_short_string(out: &mut BytesMut, s: &str) {
    let b = s.as_bytes();
    let n = b.len().min(u16::MAX as usize);
    out.put_u16(n as u16);
    out.put_slice(&b[..n]);
}

fn put_props(out: &mut BytesMut, props: &[(String, Amf0Value)]) {
    for (k, v) in props {
        put_short_string(out, k);
        encode(v, out);
    }
    out.put_slice(&[0x00, 0x00, 0x09]);
}

/// Appends the encoding of `v` to `out`.
pub fn encode(v: &Amf0Value, out: &mut BytesMut) {
    match v {
        Amf0Value::Number(n) => {
            out.put_u8(0x00);
            out.put_f64(*n);
        }
        Amf0Value::Boolean(b) => {
            out.put_u8(0x01);
            out.put_u8(u8::from(*b));
        }
        Amf0Value::String(s) if s.len() <= u16::MAX as usize => {
            out.put_u8(0x02);
            put_short_string(out, s);
        }
        Amf0Value::String(s) | Amf0Value::LongString(s) => {
            out.put_u8(0x0c);
            out.put_u32(s.len() as u32);
            out.put_slice(s.as_bytes());
        }
        Amf0Value::Object(props) => {
            out.put_u8(0x03);
            put_props(out, props);
        }
        Amf0Value::Null => out.put_u8(0x05),
        Amf0Value::Undefined => out.put_u8(0x06),
        Amf0Value::EcmaArray(props) => {
            out.put_u8(0x08);
            out.put_u32(props.len() as u32);
            put_props(out, props);
        }
        Amf0Value::StrictArray(items) => {
            out.put_u8(0x0a);
            out.put_u32(items.len() as u32);
            for item in items {
                encode(item, out);
            }
        }
        Amf0Value::Date { millis, tz } => {
            out.put_u8(0x0b);
            out.put_f64(*millis);
            out.put_u16(*tz as u16);
        }
        Amf0Value::TypedObject { class, props } => {
            out.put_u8(0x10);
            put_short_string(out, class);
            put_props(out, props);
        }
    }
}

/// Encodes a sequence of values.
pub fn encode_all(values: &[Amf0Value]) -> BytesMut {
    let mut out = BytesMut::new();
    for v in values {
        encode(v, &mut out);
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn golden_connect_command() {
        let values = vec![
            Amf0Value::string("connect"),
            Amf0Value::Number(1.0),
            Amf0Value::object([("app", Amf0Value::string("live"))]),
        ];
        let bytes = encode_all(&values);
        let expected: &[u8] = &[
            0x02, 0x00, 0x07, b'c', b'o', b'n', b'n', b'e', b'c', b't', // "connect"
            0x00, 0x3f, 0xf0, 0, 0, 0, 0, 0, 0, // 1.0
            0x03, 0x00, 0x03, b'a', b'p', b'p', 0x02, 0x00, 0x04, b'l', b'i', b'v', b'e', 0x00,
            0x00, 0x09,
        ];
        assert_eq!(&bytes[..], expected);
        assert_eq!(decode_all(expected).unwrap(), values);
    }

    #[test]
    fn round_trip_all_types() {
        let values = vec![
            Amf0Value::Number(-2.5),
            Amf0Value::Boolean(true),
            Amf0Value::Null,
            Amf0Value::Undefined,
            Amf0Value::EcmaArray(vec![("width".into(), Amf0Value::Number(1920.0))]),
            Amf0Value::StrictArray(vec![Amf0Value::string("avc1"), Amf0Value::string("hvc1")]),
            Amf0Value::Date {
                millis: 1.0e12,
                tz: 0,
            },
            Amf0Value::LongString("x".repeat(70_000)),
            Amf0Value::TypedObject {
                class: "C".into(),
                props: vec![],
            },
        ];
        let bytes = encode_all(&values);
        assert_eq!(decode_all(&bytes).unwrap(), values);
    }

    #[test]
    fn truncated_input_is_an_error_not_a_panic() {
        let bytes = encode_all(&[Amf0Value::object([("k", Amf0Value::Number(1.0))])]);
        for n in 0..bytes.len() - 3 {
            assert!(decode_all(&bytes[..n]).is_err() || n == 0);
        }
    }

    #[test]
    fn deep_nesting_is_rejected() {
        let mut data = Vec::new();
        for _ in 0..200 {
            data.extend_from_slice(&[0x0a, 0, 0, 0, 1]);
        }
        data.push(0x05);
        assert_eq!(decode_all(&data), Err(Amf0Error::TooDeep));
    }

    #[test]
    fn get_property() {
        let v = Amf0Value::object([("code", Amf0Value::string("NetStream.Publish.Start"))]);
        assert_eq!(
            v.get("code").and_then(|v| v.as_str()),
            Some("NetStream.Publish.Start")
        );
        assert!(v.get("missing").is_none());
    }
}
