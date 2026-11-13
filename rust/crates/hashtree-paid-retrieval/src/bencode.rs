//! Strict, bounded bencode for the paid-retrieval message headers.
//!
//! Only canonical input is accepted (sorted unique keys, minimal integers), so
//! every message has exactly one encoding on every carrier.

use std::collections::BTreeMap;

const MAX_DEPTH: usize = 4;

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Value {
    Int(i64),
    Bytes(Vec<u8>),
    List(Vec<Value>),
    Dict(BTreeMap<Vec<u8>, Value>),
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, thiserror::Error)]
#[error("invalid bencode: {0}")]
pub struct BencodeError(pub &'static str);

impl Value {
    pub fn as_int(&self) -> Option<i64> {
        match self {
            Value::Int(value) => Some(*value),
            _ => None,
        }
    }

    pub fn as_bytes(&self) -> Option<&[u8]> {
        match self {
            Value::Bytes(value) => Some(value),
            _ => None,
        }
    }

    pub fn as_list(&self) -> Option<&[Value]> {
        match self {
            Value::List(value) => Some(value),
            _ => None,
        }
    }
}

pub fn encode(value: &Value, out: &mut Vec<u8>) {
    match value {
        Value::Int(number) => {
            out.push(b'i');
            out.extend_from_slice(number.to_string().as_bytes());
            out.push(b'e');
        }
        Value::Bytes(bytes) => {
            out.extend_from_slice(bytes.len().to_string().as_bytes());
            out.push(b':');
            out.extend_from_slice(bytes);
        }
        Value::List(items) => {
            out.push(b'l');
            for item in items {
                encode(item, out);
            }
            out.push(b'e');
        }
        Value::Dict(entries) => {
            out.push(b'd');
            for (key, item) in entries {
                out.extend_from_slice(key.len().to_string().as_bytes());
                out.push(b':');
                out.extend_from_slice(key);
                encode(item, out);
            }
            out.push(b'e');
        }
    }
}

/// Decode one value from the start of `input`, returning it and its length.
pub fn decode_prefix(input: &[u8]) -> Result<(Value, usize), BencodeError> {
    let mut reader = Reader { input, at: 0 };
    let value = reader.value(0)?;
    Ok((value, reader.at))
}

struct Reader<'a> {
    input: &'a [u8],
    at: usize,
}

impl Reader<'_> {
    fn peek(&self) -> Result<u8, BencodeError> {
        self.input
            .get(self.at)
            .copied()
            .ok_or(BencodeError("unexpected end"))
    }

    fn value(&mut self, depth: usize) -> Result<Value, BencodeError> {
        if depth > MAX_DEPTH {
            return Err(BencodeError("nested too deeply"));
        }
        match self.peek()? {
            b'i' => {
                self.at += 1;
                let number = self.number(b'e', true)?;
                Ok(Value::Int(number))
            }
            b'l' => {
                self.at += 1;
                let mut items = Vec::new();
                while self.peek()? != b'e' {
                    items.push(self.value(depth + 1)?);
                }
                self.at += 1;
                Ok(Value::List(items))
            }
            b'd' => {
                self.at += 1;
                let mut entries = BTreeMap::new();
                let mut last: Option<Vec<u8>> = None;
                while self.peek()? != b'e' {
                    let key = self.bytes()?;
                    if last.as_ref().is_some_and(|last| *last >= key) {
                        return Err(BencodeError("dictionary keys are not sorted"));
                    }
                    let item = self.value(depth + 1)?;
                    last = Some(key.clone());
                    entries.insert(key, item);
                }
                self.at += 1;
                Ok(Value::Dict(entries))
            }
            b'0'..=b'9' => Ok(Value::Bytes(self.bytes()?)),
            _ => Err(BencodeError("unknown type")),
        }
    }

    fn bytes(&mut self) -> Result<Vec<u8>, BencodeError> {
        let length = usize::try_from(self.number(b':', false)?)
            .map_err(|_| BencodeError("string length is out of range"))?;
        let end = self
            .at
            .checked_add(length)
            .filter(|end| *end <= self.input.len())
            .ok_or(BencodeError("string is longer than the message"))?;
        let bytes = self.input[self.at..end].to_vec();
        self.at = end;
        Ok(bytes)
    }

    /// Parse a minimal decimal terminated by `end`.
    fn number(&mut self, end: u8, signed: bool) -> Result<i64, BencodeError> {
        let start = self.at;
        while self.peek()? != end {
            self.at += 1;
            if self.at - start > 20 {
                return Err(BencodeError("number is too long"));
            }
        }
        let digits = &self.input[start..self.at];
        self.at += 1;
        let (negative, magnitude) = match digits.split_first() {
            Some((b'-', rest)) if signed => (true, rest),
            _ => (false, digits),
        };
        let minimal = match magnitude {
            [] => false,
            [b'0'] => !negative,
            [b'0', ..] => false,
            _ => magnitude.iter().all(u8::is_ascii_digit),
        };
        if !minimal {
            return Err(BencodeError("number is not canonical"));
        }
        std::str::from_utf8(digits)
            .ok()
            .and_then(|text| text.parse::<i64>().ok())
            .ok_or(BencodeError("number is out of range"))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn dict(entries: &[(&str, Value)]) -> Value {
        Value::Dict(
            entries
                .iter()
                .map(|(key, value)| (key.as_bytes().to_vec(), value.clone()))
                .collect(),
        )
    }

    #[test]
    fn encodes_like_bittorrent_and_reports_the_header_length() {
        let value = dict(&[
            ("t", Value::Bytes(b"get".to_vec())),
            ("x", Value::Int(-7)),
            ("m", Value::List(vec![Value::Bytes(b"a".to_vec())])),
        ]);
        let mut encoded = Vec::new();
        encode(&value, &mut encoded);
        assert_eq!(encoded, b"d1:ml1:ae1:t3:get1:xi-7ee");
        encoded.extend_from_slice(b"raw");
        let (decoded, used) = decode_prefix(&encoded).unwrap();
        assert_eq!(decoded, value);
        assert_eq!(&encoded[used..], b"raw");
    }

    #[test]
    fn rejects_non_canonical_and_truncated_input() {
        for bad in [
            &b"i01e"[..],
            b"i-0e",
            b"ie",
            b"i1",
            b"01:a",
            b"5:abc",
            b"d1:bi1e1:ai2ee",
            b"d1:ai1e1:ai2ee",
            b"lllllli1eeeeeee",
            b"x",
            b"i99999999999999999999e",
            b"",
        ] {
            assert!(decode_prefix(bad).is_err(), "{:?}", bad);
        }
    }
}
