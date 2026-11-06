//! Read-only `bplist00` decoder for importing the Flutter app's NSUserDefaults values.
//! Covers the object kinds NSUserDefaults writes for Flutter preferences: dictionaries,
//! arrays, strings, booleans, integers, reals and dates (seconds since 2001 as a number).
use serde_json::{Map, Number, Value};

struct Plist<'a> {
    bytes: &'a [u8],
    offsets: Vec<usize>,
    ref_size: usize,
}

fn uint(bytes: &[u8]) -> Option<u64> {
    if bytes.len() > 8 {
        return None;
    }
    Some(bytes.iter().fold(0u64, |acc, b| (acc << 8) | u64::from(*b)))
}

pub(crate) fn parse(bytes: &[u8]) -> Option<Value> {
    if bytes.len() < 40 || &bytes[..8] != b"bplist00" {
        return None;
    }
    let trailer = &bytes[bytes.len() - 32..];
    let offset_size = trailer[6] as usize;
    let ref_size = trailer[7] as usize;
    let count = usize::try_from(uint(&trailer[8..16])?).ok()?;
    let top = usize::try_from(uint(&trailer[16..24])?).ok()?;
    let table = usize::try_from(uint(&trailer[24..32])?).ok()?;
    if offset_size == 0 || ref_size == 0 || count == 0 || top >= count {
        return None;
    }
    let table_end = table.checked_add(count.checked_mul(offset_size)?)?;
    if table_end > bytes.len() - 32 {
        return None;
    }
    let offsets = (0..count)
        .map(|i| {
            let start = table + i * offset_size;
            uint(&bytes[start..start + offset_size]).and_then(|v| usize::try_from(v).ok())
        })
        .collect::<Option<Vec<_>>>()?;
    let plist = Plist {
        bytes,
        offsets,
        ref_size,
    };
    plist.object(top, 0)
}

impl Plist<'_> {
    fn byte(&self, at: usize) -> Option<u8> {
        self.bytes.get(at).copied()
    }

    fn slice(&self, at: usize, len: usize) -> Option<&[u8]> {
        self.bytes.get(at..at.checked_add(len)?)
    }

    /// Length of a variable-size object and the offset of its contents.
    fn length(&self, at: usize, marker: u8) -> Option<(usize, usize)> {
        let low = marker & 0x0f;
        if low != 0x0f {
            return Some((low as usize, at + 1));
        }
        let int_marker = self.byte(at + 1)?;
        if int_marker >> 4 != 0x1 {
            return None;
        }
        let size = 1usize << (int_marker & 0x0f);
        let len = usize::try_from(uint(self.slice(at + 2, size)?)?).ok()?;
        Some((len, at + 2 + size))
    }

    fn reference(&self, at: usize) -> Option<usize> {
        usize::try_from(uint(self.slice(at, self.ref_size)?)?).ok()
    }

    fn object(&self, index: usize, depth: usize) -> Option<Value> {
        if depth > 64 {
            return None;
        }
        let at = *self.offsets.get(index)?;
        let marker = self.byte(at)?;
        match marker >> 4 {
            0x0 => match marker {
                0x00 => Some(Value::Null),
                0x08 => Some(Value::Bool(false)),
                0x09 => Some(Value::Bool(true)),
                _ => None,
            },
            0x1 => {
                let size = 1usize << (marker & 0x0f);
                let raw = uint(self.slice(at + 1, size)?)?;
                // 8-byte integers are signed; 16-byte ones are not produced for preferences.
                Some(if size == 8 {
                    Value::Number((raw as i64).into())
                } else {
                    Value::Number(raw.into())
                })
            }
            0x2 | 0x3 => {
                let size = if marker >> 4 == 0x3 {
                    8
                } else {
                    1usize << (marker & 0x0f)
                };
                let raw = self.slice(at + 1, size)?;
                let value = match size {
                    4 => f64::from(f32::from_be_bytes(raw.try_into().ok()?)),
                    8 => f64::from_be_bytes(raw.try_into().ok()?),
                    _ => return None,
                };
                Number::from_f64(value).map(Value::Number)
            }
            0x5 => {
                let (len, start) = self.length(at, marker)?;
                let raw = self.slice(start, len)?;
                Some(Value::String(raw.iter().map(|b| char::from(*b)).collect()))
            }
            0x6 => {
                let (len, start) = self.length(at, marker)?;
                let raw = self.slice(start, len.checked_mul(2)?)?;
                let units: Vec<u16> = raw
                    .chunks_exact(2)
                    .map(|pair| u16::from_be_bytes([pair[0], pair[1]]))
                    .collect();
                String::from_utf16(&units).ok().map(Value::String)
            }
            0xA => {
                let (len, start) = self.length(at, marker)?;
                (0..len)
                    .map(|i| self.object(self.reference(start + i * self.ref_size)?, depth + 1))
                    .collect::<Option<Vec<_>>>()
                    .map(Value::Array)
            }
            0xD => {
                let (len, start) = self.length(at, marker)?;
                let mut map = Map::new();
                for i in 0..len {
                    let key = self.object(self.reference(start + i * self.ref_size)?, depth + 1)?;
                    let value = self.object(
                        self.reference(start + (len + i) * self.ref_size)?,
                        depth + 1,
                    )?;
                    map.insert(key.as_str()?.to_owned(), value);
                }
                Some(Value::Object(map))
            }
            // Data, UIDs and sets have no Flutter preference meaning.
            _ => Some(Value::Null),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// `{"flutter.paseo.clientId": "abc", "flutter.flag": true}` as written by `plutil`.
    fn sample() -> Vec<u8> {
        let mut bytes = b"bplist00".to_vec();
        let mut offsets = Vec::new();
        // 0: dict with 2 entries, keys 1,2 values 3,4
        offsets.push(bytes.len());
        bytes.extend_from_slice(&[0xD2, 1, 2, 3, 4]);
        offsets.push(bytes.len());
        bytes.push(0x5F);
        bytes.extend_from_slice(&[0x10, 21]);
        bytes.extend_from_slice(b"flutter.paseo.clientI");
        offsets.push(bytes.len());
        bytes.push(0x5C);
        bytes.extend_from_slice(b"flutter.flag");
        offsets.push(bytes.len());
        bytes.push(0x63);
        bytes.extend_from_slice(&[0, b'a', 0, b'b', 0, b'c']);
        offsets.push(bytes.len());
        bytes.push(0x09);
        let table = bytes.len();
        for offset in &offsets {
            bytes.push(*offset as u8);
        }
        let mut trailer = [0u8; 32];
        trailer[6] = 1;
        trailer[7] = 1;
        trailer[15] = offsets.len() as u8;
        trailer[23] = 0;
        trailer[31] = table as u8;
        bytes.extend_from_slice(&trailer);
        bytes
    }

    #[test]
    fn decodes_dictionaries_of_strings_and_booleans() {
        let value = parse(&sample()).unwrap();
        assert_eq!(value["flutter.paseo.clientI"], "abc");
        assert_eq!(value["flutter.flag"], true);
        assert!(parse(b"not a plist").is_none());
    }
}
