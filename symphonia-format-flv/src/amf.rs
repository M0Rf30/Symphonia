// Symphonia
// Copyright (c) 2019-2026 The Project Symphonia Developers.
//
// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! A parser for the subset of the Action Message Format 0 (AMF0) of FLV script data tags.

/// The maximum depth of nested objects and arrays.
const MAX_DEPTH: u32 = 8;

/// The maximum number of elements read from an array or object.
const MAX_ELEMENTS: usize = 1 << 20;

/// An AMF0 value.
#[derive(Clone, Debug, PartialEq)]
pub enum Amf {
    Number(f64),
    Bool(bool),
    String(String),
    /// An object or ECMA array: an ordered list of properties.
    Object(Vec<(String, Amf)>),
    /// A strict array.
    Array(Vec<Amf>),
    Null,
}

impl Amf {
    /// Get a property of an object.
    pub fn get(&self, key: &str) -> Option<&Amf> {
        match self {
            Amf::Object(props) => props.iter().find(|(k, _)| k == key).map(|(_, v)| v),
            _ => None,
        }
    }

    pub fn as_f64(&self) -> Option<f64> {
        match self {
            Amf::Number(n) => Some(*n),
            _ => None,
        }
    }

    pub fn as_str(&self) -> Option<&str> {
        match self {
            Amf::String(s) => Some(s),
            _ => None,
        }
    }
}

struct Cursor<'a> {
    buf: &'a [u8],
}

impl<'a> Cursor<'a> {
    fn take(&mut self, n: usize) -> Option<&'a [u8]> {
        let (head, tail) = (self.buf.get(..n)?, self.buf.get(n..)?);
        self.buf = tail;
        Some(head)
    }

    fn u8(&mut self) -> Option<u8> {
        Some(self.take(1)?[0])
    }

    fn u16(&mut self) -> Option<u16> {
        Some(u16::from_be_bytes(self.take(2)?.try_into().ok()?))
    }

    fn u32(&mut self) -> Option<u32> {
        Some(u32::from_be_bytes(self.take(4)?.try_into().ok()?))
    }

    fn f64(&mut self) -> Option<f64> {
        Some(f64::from_be_bytes(self.take(8)?.try_into().ok()?))
    }

    fn string(&mut self, len: usize) -> Option<String> {
        Some(String::from_utf8_lossy(self.take(len)?).into_owned())
    }

    fn properties(&mut self, depth: u32) -> Option<Vec<(String, Amf)>> {
        let mut props = vec![];

        loop {
            let len = usize::from(self.u16()?);

            if len == 0 {
                // The object end marker follows an empty key. Be lenient about its absence at the
                // end of the data.
                if self.buf.first() == Some(&0x09) {
                    self.take(1)?;
                }
                return Some(props);
            }

            let key = self.string(len)?;
            let value = self.value(depth + 1)?;

            if props.len() >= MAX_ELEMENTS {
                return None;
            }

            props.push((key, value));
        }
    }

    fn value(&mut self, depth: u32) -> Option<Amf> {
        if depth > MAX_DEPTH {
            return None;
        }

        Some(match self.u8()? {
            0x00 => Amf::Number(self.f64()?),
            0x01 => Amf::Bool(self.u8()? != 0),
            0x02 => {
                let len = usize::from(self.u16()?);
                Amf::String(self.string(len)?)
            }
            0x03 => Amf::Object(self.properties(depth)?),
            // Null, undefined.
            0x05 | 0x06 => Amf::Null,
            // ECMA array: the (approximate) count of properties is followed by the properties.
            0x08 => {
                self.u32()?;
                Amf::Object(self.properties(depth)?)
            }
            // Strict array.
            0x0a => {
                let count = self.u32()? as usize;

                if count > MAX_ELEMENTS {
                    return None;
                }

                let mut items = Vec::with_capacity(count.min(1024));

                for _ in 0..count {
                    items.push(self.value(depth + 1)?);
                }

                Amf::Array(items)
            }
            // Date: milliseconds since the epoch and the time zone.
            0x0b => {
                let ms = self.f64()?;
                self.u16()?;
                Amf::Number(ms)
            }
            // Long string.
            0x0c => {
                let len = self.u32()? as usize;
                Amf::String(self.string(len)?)
            }
            _ => return None,
        })
    }
}

/// Parse the values of a script data tag. Parsing stops at the first value that cannot be parsed.
pub fn parse_values(buf: &[u8]) -> Vec<Amf> {
    let mut cursor = Cursor { buf };
    let mut values = vec![];

    while !cursor.buf.is_empty() {
        match cursor.value(0) {
            Some(v) => values.push(v),
            None => break,
        }
    }

    values
}

#[cfg(test)]
mod tests {
    use super::*;

    fn string(s: &str) -> Vec<u8> {
        let mut v = vec![0x02];
        v.extend_from_slice(&(s.len() as u16).to_be_bytes());
        v.extend_from_slice(s.as_bytes());
        v
    }

    fn prop(key: &str, value: &[u8]) -> Vec<u8> {
        let mut v = (key.len() as u16).to_be_bytes().to_vec();
        v.extend_from_slice(key.as_bytes());
        v.extend_from_slice(value);
        v
    }

    fn number(n: f64) -> Vec<u8> {
        let mut v = vec![0x00];
        v.extend_from_slice(&n.to_be_bytes());
        v
    }

    #[test]
    fn verify_on_metadata() {
        let mut data = string("onMetaData");
        data.push(0x08);
        data.extend_from_slice(&3u32.to_be_bytes());
        data.extend(prop("duration", &number(12.5)));
        data.extend(prop("encoder", &string("test")));

        let mut arr = vec![0x0a, 0, 0, 0, 2];
        arr.extend(number(1.0));
        arr.extend(number(2.0));
        let mut kf = vec![0x03];
        kf.extend(prop("times", &arr));
        kf.extend_from_slice(&[0, 0, 9]);
        data.extend(prop("keyframes", &kf));
        data.extend_from_slice(&[0, 0, 9]);

        let values = parse_values(&data);
        assert_eq!(values.len(), 2);
        assert_eq!(values[0].as_str(), Some("onMetaData"));
        assert_eq!(values[1].get("duration").and_then(Amf::as_f64), Some(12.5));
        assert_eq!(values[1].get("encoder").and_then(Amf::as_str), Some("test"));
        assert_eq!(
            values[1].get("keyframes").and_then(|k| k.get("times")),
            Some(&Amf::Array(vec![Amf::Number(1.0), Amf::Number(2.0)]))
        );
    }

    #[test]
    fn verify_truncated_and_malformed_data() {
        let mut data = string("onMetaData");
        data.push(0x03);
        data.extend(prop("duration", &number(1.0)));
        // Truncated.
        assert_eq!(parse_values(&data).len(), 1);
        assert!(parse_values(&[0x03, 0xff]).is_empty());
        assert!(parse_values(&[0x77]).is_empty());

        // Deeply nested objects are rejected rather than overflowing the stack.
        let mut nested = vec![];
        for _ in 0..100 {
            nested.push(0x03);
            nested.extend_from_slice(&[0, 1, b'a']);
        }
        assert!(parse_values(&nested).is_empty());
    }
}
