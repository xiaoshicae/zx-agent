//! Frames of the long connection: the protobuf message `pbbp2.Frame` of the official Go SDK
//! (`ws/pbbp2.pb.go`), encoded by hand since it is one small message:
//!
//! ```text
//! Frame  { 1 SeqID uint64, 2 LogID uint64, 3 service int32, 4 method int32,
//!          5 headers repeated Header, 6 payload_encoding string, 7 payload_type string,
//!          8 payload bytes, 9 LogIDNew string }
//! Header { 1 key string, 2 value string }
//! ```
//!
//! Every field is written, zero values included, so that a decoder that requires them is
//! satisfied; unknown fields are skipped when reading.

use std::fmt;

const VARINT: u64 = 0;
const FIXED64: u64 = 1;
const LENGTH_DELIMITED: u64 = 2;
const FIXED32: u64 = 5;

#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub(crate) struct Frame {
    pub(crate) seq_id: u64,
    pub(crate) log_id: u64,
    pub(crate) service: i32,
    pub(crate) method: i32,
    pub(crate) headers: Vec<(String, String)>,
    pub(crate) payload_encoding: String,
    pub(crate) payload_type: String,
    pub(crate) payload: Vec<u8>,
    pub(crate) log_id_new: String,
}

/// Why bytes are not a frame.
#[derive(Debug, PartialEq, Eq)]
pub(crate) struct FrameError(&'static str);

impl fmt::Display for FrameError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "malformed frame: {}", self.0)
    }
}

impl std::error::Error for FrameError {}

impl Frame {
    /// The value of the first header named `key`.
    pub(crate) fn header(&self, key: &str) -> Option<&str> {
        self.headers
            .iter()
            .find(|(name, _)| name == key)
            .map(|(_, value)| value.as_str())
    }

    pub(crate) fn encode(&self) -> Vec<u8> {
        let mut out = Vec::with_capacity(self.payload.len() + 64);
        put_varint_field(&mut out, 1, self.seq_id);
        put_varint_field(&mut out, 2, self.log_id);
        // int32 is a varint of the value sign-extended to 64 bits.
        put_varint_field(&mut out, 3, i64::from(self.service) as u64);
        put_varint_field(&mut out, 4, i64::from(self.method) as u64);
        for (key, value) in &self.headers {
            let mut header = Vec::with_capacity(key.len() + value.len() + 4);
            put_bytes_field(&mut header, 1, key.as_bytes());
            put_bytes_field(&mut header, 2, value.as_bytes());
            put_bytes_field(&mut out, 5, &header);
        }
        put_bytes_field(&mut out, 6, self.payload_encoding.as_bytes());
        put_bytes_field(&mut out, 7, self.payload_type.as_bytes());
        put_bytes_field(&mut out, 8, &self.payload);
        put_bytes_field(&mut out, 9, self.log_id_new.as_bytes());
        out
    }

    pub(crate) fn decode(bytes: &[u8]) -> Result<Self, FrameError> {
        let mut frame = Frame::default();
        let mut reader = Reader(bytes);
        while !reader.0.is_empty() {
            let key = reader.varint()?;
            match (key >> 3, key & 7) {
                (1, VARINT) => frame.seq_id = reader.varint()?,
                (2, VARINT) => frame.log_id = reader.varint()?,
                // An int32 field keeps the low 32 bits of its varint.
                (3, VARINT) => frame.service = reader.varint()? as u32 as i32,
                (4, VARINT) => frame.method = reader.varint()? as u32 as i32,
                (5, LENGTH_DELIMITED) => frame.headers.push(decode_header(reader.bytes()?)?),
                (6, LENGTH_DELIMITED) => frame.payload_encoding = text(reader.bytes()?)?,
                (7, LENGTH_DELIMITED) => frame.payload_type = text(reader.bytes()?)?,
                (8, LENGTH_DELIMITED) => frame.payload = reader.bytes()?.to_vec(),
                (9, LENGTH_DELIMITED) => frame.log_id_new = text(reader.bytes()?)?,
                (_, wire) => reader.skip(wire)?,
            }
        }
        Ok(frame)
    }
}

fn decode_header(bytes: &[u8]) -> Result<(String, String), FrameError> {
    let mut header = (String::new(), String::new());
    let mut reader = Reader(bytes);
    while !reader.0.is_empty() {
        let key = reader.varint()?;
        match (key >> 3, key & 7) {
            (1, LENGTH_DELIMITED) => header.0 = text(reader.bytes()?)?,
            (2, LENGTH_DELIMITED) => header.1 = text(reader.bytes()?)?,
            (_, wire) => reader.skip(wire)?,
        }
    }
    Ok(header)
}

fn text(bytes: &[u8]) -> Result<String, FrameError> {
    String::from_utf8(bytes.to_vec()).map_err(|_| FrameError("a string field is not UTF-8"))
}

fn put_varint(out: &mut Vec<u8>, mut value: u64) {
    while value >= 0x80 {
        // The low seven bits, with the continuation bit set; the cast keeps exactly them.
        out.push((value as u8 & 0x7f) | 0x80);
        value >>= 7;
    }
    // Below 0x80 now, so it fits in one byte.
    out.push(value as u8);
}

fn put_varint_field(out: &mut Vec<u8>, field: u64, value: u64) {
    put_varint(out, (field << 3) | VARINT);
    put_varint(out, value);
}

fn put_bytes_field(out: &mut Vec<u8>, field: u64, bytes: &[u8]) {
    put_varint(out, (field << 3) | LENGTH_DELIMITED);
    // usize fits in u64 on every supported target.
    put_varint(out, bytes.len() as u64);
    out.extend_from_slice(bytes);
}

struct Reader<'a>(&'a [u8]);

impl<'a> Reader<'a> {
    fn varint(&mut self) -> Result<u64, FrameError> {
        let mut value = 0u64;
        for shift in (0..64).step_by(7) {
            let (&byte, rest) = self.0.split_first().ok_or(FrameError("a varint is cut off"))?;
            self.0 = rest;
            value |= u64::from(byte & 0x7f) << shift;
            if byte & 0x80 == 0 {
                return Ok(value);
            }
        }
        Err(FrameError("a varint is longer than ten bytes"))
    }

    fn bytes(&mut self) -> Result<&'a [u8], FrameError> {
        let length = usize::try_from(self.varint()?).map_err(|_| FrameError("a length does not fit"))?;
        if length > self.0.len() {
            return Err(FrameError("a field is cut off"));
        }
        let (field, rest) = self.0.split_at(length);
        self.0 = rest;
        Ok(field)
    }

    fn skip(&mut self, wire: u64) -> Result<(), FrameError> {
        let width = match wire {
            VARINT => return self.varint().map(drop),
            LENGTH_DELIMITED => return self.bytes().map(drop),
            FIXED64 => 8,
            FIXED32 => 4,
            _ => return Err(FrameError("an unsupported wire type")),
        };
        if width > self.0.len() {
            return Err(FrameError("a field is cut off"));
        }
        self.0 = &self.0[width..];
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sample() -> Frame {
        Frame {
            seq_id: 300,
            log_id: 1,
            service: 7,
            method: 1,
            headers: vec![("type".into(), "event".into()), ("sum".into(), "1".into())],
            payload_encoding: String::new(),
            payload_type: String::new(),
            payload: br#"{"a":1}"#.to_vec(),
            log_id_new: "n".into(),
        }
    }

    #[test]
    fn a_frame_survives_encoding_and_decoding() {
        assert_eq!(Frame::decode(&sample().encode()).unwrap(), sample());
    }

    #[test]
    fn the_encoding_follows_the_protobuf_wire_format() {
        let frame = Frame {
            seq_id: 300,
            service: -1,
            headers: vec![("k".into(), "v".into())],
            payload: vec![0xff],
            ..Frame::default()
        };
        let mut expected = vec![0x08, 0xac, 0x02, 0x10, 0x00, 0x18];
        expected.extend([0xff; 9]);
        expected.push(0x01);
        expected.extend([0x20, 0x00, 0x2a, 0x06, 0x0a, 0x01, b'k', 0x12, 0x01, b'v']);
        expected.extend([0x32, 0x00, 0x3a, 0x00, 0x42, 0x01, 0xff, 0x4a, 0x00]);
        assert_eq!(frame.encode(), expected);
        assert_eq!(Frame::decode(&expected).unwrap().service, -1);
    }

    #[test]
    fn unknown_fields_are_skipped() {
        // Field 15 as a varint, then field 16 as three length-delimited bytes.
        let mut bytes = vec![0x78, 0x05, 0x82, 0x01, 0x03, b'x', b'y', b'z'];
        bytes.extend(sample().encode());
        assert_eq!(Frame::decode(&bytes).unwrap(), sample());
    }

    #[test]
    fn cut_off_or_malformed_bytes_are_errors() {
        let encoded = sample().encode();
        assert!(Frame::decode(&encoded[..encoded.len() - 1]).is_err());
        assert!(Frame::decode(&[0x08]).is_err());
        assert!(Frame::decode(&[0x0b]).is_err(), "wire type 3 is not supported");
        assert!(Frame::decode(&[0x08, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0x01]).is_err());
    }

    #[test]
    fn headers_are_looked_up_by_name() {
        assert_eq!(sample().header("type"), Some("event"));
        assert_eq!(sample().header("missing"), None);
    }
}
