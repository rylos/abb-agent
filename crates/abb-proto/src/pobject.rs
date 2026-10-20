//! PObject: the big-endian TLV format ABB agent and server use to exchange messages.
//!
//! Each message is a single serialized PObject, with no header or length (PStream).
//!
//! | tag  | type   | encoding                                   |
//! |------|--------|--------------------------------------------|
//! | 0x00 | null   | `00 00` (tag + length byte, always 0)      |
//! | 0x01 | integer| `01 <n:u8> <n bytes BE>`, n = 1, 2, 4 or 8 |
//! | 0x10 | string | `10 <len:u16 BE> <bytes>`                  |
//! | 0x20 | buffer | `20 <len:u32 BE> <bytes>`                  |
//! | 0x41 | array  | `41 <objects…> 40`                         |
//! | 0x42 | map    | `42 (<key 0x10> <value>)* 40`              |

use std::fmt;
use std::io::{self, Read};

const TAG_NULL: u8 = 0x00;
const TAG_INT: u8 = 0x01;
const TAG_STRING: u8 = 0x10;
const TAG_BUFFER: u8 = 0x20;
const TAG_END: u8 = 0x40;
const TAG_ARRAY: u8 = 0x41;
const TAG_MAP: u8 = 0x42;

#[derive(Clone, PartialEq, Eq)]
pub enum Value {
    Null,
    /// The server treats integers as u64; bools are 0/1.
    Int(u64),
    /// Strings are arbitrary bytes, almost always UTF-8.
    String(Vec<u8>),
    Buffer(Vec<u8>),
    Array(Vec<Value>),
    /// Keys keep their on-the-wire order.
    Map(Vec<(String, Value)>),
}

#[derive(Debug, PartialEq, Eq)]
pub enum DecodeError {
    /// More bytes needed: the message is not complete yet.
    Incomplete,
    BadTag { tag: u8, offset: usize },
    BadIntWidth { width: u8, offset: usize },
    BadNullLength { width: u8, offset: usize },
    KeyNotString { offset: usize },
}

impl fmt::Display for DecodeError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Incomplete => write!(f, "incomplete message"),
            Self::BadTag { tag, offset } => write!(f, "unknown tag 0x{tag:02x} at offset {offset}"),
            Self::BadIntWidth { width, offset } => {
                write!(f, "invalid integer width {width} at offset {offset}")
            }
            Self::BadNullLength { width, offset } => {
                write!(f, "null with length {width} (expected 0) at offset {offset}")
            }
            Self::KeyNotString { offset } => write!(f, "non-string map key at offset {offset}"),
        }
    }
}

impl std::error::Error for DecodeError {}

impl Value {
    pub fn str(s: impl Into<String>) -> Self {
        Value::String(s.into().into_bytes())
    }

    pub fn map<K: Into<String>>(entries: impl IntoIterator<Item = (K, Value)>) -> Self {
        Value::Map(entries.into_iter().map(|(k, v)| (k.into(), v)).collect())
    }

    /// Value of a key, if `self` is a map.
    pub fn get(&self, key: &str) -> Option<&Value> {
        match self {
            Value::Map(entries) => entries.iter().find(|(k, _)| k == key).map(|(_, v)| v),
            _ => None,
        }
    }

    pub fn as_int(&self) -> Option<u64> {
        match self {
            Value::Int(n) => Some(*n),
            _ => None,
        }
    }

    pub fn as_str(&self) -> Option<&str> {
        match self {
            Value::String(s) => std::str::from_utf8(s).ok(),
            _ => None,
        }
    }

    pub fn encode(&self, out: &mut Vec<u8>) {
        match self {
            Value::Null => out.extend_from_slice(&[TAG_NULL, 0]),
            Value::Int(n) => {
                out.push(TAG_INT);
                let bytes = n.to_be_bytes();
                let width = match *n {
                    0..=0xff => 1,
                    0x100..=0xffff => 2,
                    0x1_0000..=0xffff_ffff => 4,
                    _ => 8,
                };
                out.push(width as u8);
                out.extend_from_slice(&bytes[8 - width..]);
            }
            Value::String(s) => encode_string(s, out),
            Value::Buffer(b) => {
                out.push(TAG_BUFFER);
                out.extend_from_slice(&(b.len() as u32).to_be_bytes());
                out.extend_from_slice(b);
            }
            Value::Array(items) => {
                out.push(TAG_ARRAY);
                for item in items {
                    item.encode(out);
                }
                out.push(TAG_END);
            }
            Value::Map(entries) => {
                out.push(TAG_MAP);
                for (k, v) in entries {
                    encode_string(k.as_bytes(), out);
                    v.encode(out);
                }
                out.push(TAG_END);
            }
        }
    }

    pub fn to_bytes(&self) -> Vec<u8> {
        let mut out = Vec::new();
        self.encode(&mut out);
        out
    }

    /// Reads a complete PObject from a stream (messages have no framing).
    pub fn read_from(r: &mut impl Read) -> io::Result<Value> {
        let tag = read_u8(r)?;
        read_with_tag(r, tag)
    }

    /// Decodes a PObject from the start of `buf`; returns the value and the bytes consumed.
    pub fn decode(buf: &[u8]) -> Result<(Value, usize), DecodeError> {
        let mut d = Decoder { buf, pos: 0 };
        let v = d.value()?;
        Ok((v, d.pos))
    }
}

fn read_u8(r: &mut impl Read) -> io::Result<u8> {
    let mut b = [0u8; 1];
    r.read_exact(&mut b)?;
    Ok(b[0])
}

fn read_vec(r: &mut impl Read, len: usize) -> io::Result<Vec<u8>> {
    let mut v = vec![0u8; len];
    r.read_exact(&mut v)?;
    Ok(v)
}

fn bad(msg: String) -> io::Error {
    io::Error::new(io::ErrorKind::InvalidData, msg)
}

fn read_with_tag(r: &mut impl Read, tag: u8) -> io::Result<Value> {
    Ok(match tag {
        TAG_NULL => match read_u8(r)? {
            0 => Value::Null,
            n => return Err(bad(format!("null with length {n}"))),
        },
        TAG_INT => {
            let width = read_u8(r)?;
            if !matches!(width, 1 | 2 | 4 | 8) {
                return Err(bad(format!("integer width {width}")));
            }
            Value::Int(read_vec(r, width as usize)?.iter().fold(0u64, |n, &b| n << 8 | b as u64))
        }
        TAG_STRING => {
            let mut l = [0u8; 2];
            r.read_exact(&mut l)?;
            Value::String(read_vec(r, u16::from_be_bytes(l) as usize)?)
        }
        TAG_BUFFER => {
            let mut l = [0u8; 4];
            r.read_exact(&mut l)?;
            Value::Buffer(read_vec(r, u32::from_be_bytes(l) as usize)?)
        }
        TAG_ARRAY => {
            let mut items = Vec::new();
            loop {
                match read_u8(r)? {
                    TAG_END => break,
                    t => items.push(read_with_tag(r, t)?),
                }
            }
            Value::Array(items)
        }
        TAG_MAP => {
            let mut entries = Vec::new();
            loop {
                match read_u8(r)? {
                    TAG_END => break,
                    TAG_STRING => {
                        let Value::String(k) = read_with_tag(r, TAG_STRING)? else { unreachable!() };
                        entries.push((String::from_utf8_lossy(&k).into_owned(), Value::read_from(r)?));
                    }
                    t => return Err(bad(format!("map key with tag 0x{t:02x}"))),
                }
            }
            Value::Map(entries)
        }
        t => return Err(bad(format!("unknown tag 0x{t:02x}"))),
    })
}

fn encode_string(s: &[u8], out: &mut Vec<u8>) {
    out.push(TAG_STRING);
    out.extend_from_slice(&(s.len() as u16).to_be_bytes());
    out.extend_from_slice(s);
}

struct Decoder<'a> {
    buf: &'a [u8],
    pos: usize,
}

impl Decoder<'_> {
    fn take(&mut self, n: usize) -> Result<&[u8], DecodeError> {
        let end = self.pos.checked_add(n).ok_or(DecodeError::Incomplete)?;
        let s = self.buf.get(self.pos..end).ok_or(DecodeError::Incomplete)?;
        self.pos = end;
        Ok(s)
    }

    fn byte(&mut self) -> Result<u8, DecodeError> {
        Ok(self.take(1)?[0])
    }

    fn value(&mut self) -> Result<Value, DecodeError> {
        let at = self.pos;
        let tag = self.byte()?;
        self.value_with_tag(tag, at)
    }

    fn value_with_tag(&mut self, tag: u8, at: usize) -> Result<Value, DecodeError> {
        Ok(match tag {
            TAG_NULL => match self.byte()? {
                0 => Value::Null,
                width => return Err(DecodeError::BadNullLength { width, offset: at }),
            },
            TAG_INT => {
                let width = self.byte()?;
                if !matches!(width, 1 | 2 | 4 | 8) {
                    return Err(DecodeError::BadIntWidth { width, offset: at });
                }
                let mut n = 0u64;
                for &b in self.take(width as usize)? {
                    n = n << 8 | b as u64;
                }
                Value::Int(n)
            }
            TAG_STRING => Value::String(self.string_body()?),
            TAG_BUFFER => {
                let len = u32::from_be_bytes(self.take(4)?.try_into().unwrap()) as usize;
                Value::Buffer(self.take(len)?.to_vec())
            }
            TAG_ARRAY => {
                let mut items = Vec::new();
                loop {
                    let at = self.pos;
                    match self.byte()? {
                        TAG_END => break,
                        t => items.push(self.value_with_tag(t, at)?),
                    }
                }
                Value::Array(items)
            }
            TAG_MAP => {
                let mut entries = Vec::new();
                loop {
                    let at = self.pos;
                    match self.byte()? {
                        TAG_END => break,
                        TAG_STRING => {
                            let key = String::from_utf8_lossy(&self.string_body()?).into_owned();
                            entries.push((key, self.value()?));
                        }
                        _ => return Err(DecodeError::KeyNotString { offset: at }),
                    }
                }
                Value::Map(entries)
            }
            tag => return Err(DecodeError::BadTag { tag, offset: at }),
        })
    }

    fn string_body(&mut self) -> Result<Vec<u8>, DecodeError> {
        let len = u16::from_be_bytes(self.take(2)?.try_into().unwrap()) as usize;
        Ok(self.take(len)?.to_vec())
    }
}

/// Human-readable JSON-like representation; long buffers are truncated.
impl fmt::Debug for Value {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Value::Null => write!(f, "null"),
            Value::Int(n) => write!(f, "{n}"),
            Value::String(s) => match std::str::from_utf8(s) {
                Ok(s) => write!(f, "{s:?}"),
                Err(_) => write!(f, "<str {} bytes, not UTF-8>", s.len()),
            },
            Value::Buffer(b) => {
                let head: String = b.iter().take(16).map(|x| format!("{x:02x}")).collect();
                write!(f, "<buf {} bytes: {head}{}>", b.len(), if b.len() > 16 { "…" } else { "" })
            }
            Value::Array(items) => f.debug_list().entries(items).finish(),
            Value::Map(entries) => {
                let mut m = f.debug_map();
                for (k, v) in entries {
                    m.entry(&format_args!("{k}"), v);
                }
                m.finish()
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn roundtrip() {
        let v = Value::map([
            ("action", Value::str("auth")),
            ("n", Value::Int(0x1_0000)),
            ("big", Value::Int(u64::MAX)),
            ("buf", Value::Buffer(vec![1, 2, 3])),
            ("list", Value::Array(vec![Value::Null, Value::Int(1)])),
        ]);
        let bytes = v.to_bytes();
        assert_eq!(Value::decode(&bytes), Ok((v, bytes.len())));
    }

    #[test]
    fn int_widths() {
        assert_eq!(Value::Int(5).to_bytes(), [0x01, 1, 5]);
        assert_eq!(Value::Int(0x1234).to_bytes(), [0x01, 2, 0x12, 0x34]);
    }

    #[test]
    fn null_has_length_byte() {
        // As sent by abb-cli -c: "otp" → 00 00
        let bytes = [0x42, 0x10, 0, 3, b'o', b't', b'p', 0, 0, 0x40];
        let v = Value::map([("otp", Value::Null)]);
        assert_eq!(v.to_bytes(), bytes);
        assert_eq!(Value::decode(&bytes), Ok((v, bytes.len())));
    }

    #[test]
    fn read_from_stream() {
        let v = Value::map([("a", Value::Array(vec![Value::Null, Value::str("x")])), ("b", Value::Buffer(vec![9; 3]))]);
        let mut bytes = v.to_bytes();
        bytes.extend(Value::Int(7).to_bytes());
        let mut cur = std::io::Cursor::new(bytes);
        assert_eq!(Value::read_from(&mut cur).unwrap(), v);
        assert_eq!(Value::read_from(&mut cur).unwrap(), Value::Int(7));
    }

    #[test]
    fn incomplete() {
        let bytes = Value::str("agent").to_bytes();
        assert_eq!(Value::decode(&bytes[..3]), Err(DecodeError::Incomplete));
    }
}
