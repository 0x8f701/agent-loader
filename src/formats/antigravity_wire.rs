//! Bounded protobuf reader and writer for Antigravity trajectory blobs.
//!
//! Malformed tags, truncated lengths, and non-UTF-8 strings are errors.
//! Unknown fields stay in document order so a workspace rewrite can splice one
//! field without rebuilding the rest of a native message.

use anyhow::{Result, bail};



#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum Wire {
    Varint(u64),
    Fixed64([u8; 8]),
    Bytes(Vec<u8>),
    Fixed32([u8; 4]),
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct Field {
    pub number: u32,
    pub value: Wire,

}

pub(crate) fn decode(bytes: &[u8]) -> Result<Vec<Field>> {
    let mut fields = Vec::new();
    let mut offset = 0;
    while offset < bytes.len() {
        let (tag, next) = read_varint(bytes, offset)?;
        if tag > u64::from(u32::MAX) || tag >> 3 == 0 {
            bail!("invalid protobuf field tag");
        }
        let number = (tag >> 3) as u32;
        let (value, end) = match tag & 7 {
            0 => {
                let (value, end) = read_varint(bytes, next)?;
                (Wire::Varint(value), end)
            }
            1 => (Wire::Fixed64(fixed(bytes, next, 8)?), next + 8),
            2 => {
                let (length, body) = read_varint(bytes, next)?;
                let length = usize::try_from(length).map_err(|_| anyhow::anyhow!("protobuf length exceeds memory"))?;
                let end = body.checked_add(length).ok_or_else(|| anyhow::anyhow!("protobuf length overflow"))?;
                if end > bytes.len() {
                    bail!("truncated protobuf length-delimited field");
                }
                (Wire::Bytes(bytes[body..end].to_vec()), end)
            }
            5 => (Wire::Fixed32(fixed(bytes, next, 4)?), next + 4),
            wire => bail!("unsupported protobuf wire type {wire}"),
        };
        fields.push(Field { number, value });
        offset = end;
    }
    Ok(fields)
}

fn fixed<const N: usize>(bytes: &[u8], offset: usize, len: usize) -> Result<[u8; N]> {
    let end = offset.checked_add(len).ok_or_else(|| anyhow::anyhow!("protobuf length overflow"))?;
    if end > bytes.len() {
        bail!("truncated protobuf fixed field");
    }
    let mut value = [0; N];
    value.copy_from_slice(&bytes[offset..end]);
    Ok(value)
}

fn read_varint(bytes: &[u8], mut offset: usize) -> Result<(u64, usize)> {
    let mut value = 0_u64;
    for shift in (0..64).step_by(7) {
        if offset >= bytes.len() {
            bail!("truncated protobuf varint");
        }

        let byte = bytes[offset];
        offset += 1;
        if shift == 63 && byte > 1 {
            bail!("protobuf varint overflows u64");
        }
        value |= u64::from(byte & 0x7f) << shift;
        if byte & 0x80 == 0 {
            return Ok((value, offset));
        }
    }
    bail!("protobuf varint is too long")
}

pub(crate) fn encode(fields: &[Field]) -> Vec<u8> {
    let mut out = Vec::new();
    for field in fields {
        match &field.value {
            Wire::Varint(value) => varint_field(&mut out, field.number, *value),
            Wire::Bytes(value) => bytes_field(&mut out, field.number, value),
            Wire::Fixed64(value) => fixed_field(&mut out, field.number, 1, value),
            Wire::Fixed32(value) => fixed_field(&mut out, field.number, 5, value),
        }
    }
    out
}

fn fixed_field(out: &mut Vec<u8>, field: u32, wire: u8, value: &[u8]) {
    write_varint(out, (u64::from(field) << 3) | u64::from(wire));
    out.extend(value);
}

pub(crate) fn varint_field(out: &mut Vec<u8>, field: u32, value: u64) {
    write_varint(out, u64::from(field) << 3);
    write_varint(out, value);
}

pub(crate) fn bytes_field(out: &mut Vec<u8>, field: u32, value: &[u8]) {
    write_varint(out, (u64::from(field) << 3) | 2);
    write_varint(out, value.len() as u64);
    out.extend(value);
}

pub(crate) fn string_field(out: &mut Vec<u8>, field: u32, value: &str) {
    if !value.is_empty() {
        bytes_field(out, field, value.as_bytes());
    }
}

pub(crate) fn message_field(out: &mut Vec<u8>, field: u32, body: &[u8]) {
    if !body.is_empty() {
        bytes_field(out, field, body);
    }
}

fn write_varint(out: &mut Vec<u8>, mut value: u64) {
    while value >= 0x80 {
        out.push((value as u8) | 0x80);
        value >>= 7;
    }
    out.push(value as u8);
}

pub(crate) fn varint(fields: &[Field], number: u32) -> Option<u64> {
    fields.iter().find_map(|field| match field {
        Field { number: found, value: Wire::Varint(value), .. } if *found == number => Some(*value),
        _ => None,
    })
}

pub(crate) fn bytes<'a>(fields: &'a [Field], number: u32) -> Option<&'a [u8]> {
    fields.iter().find_map(|field| match field {
        Field { number: found, value: Wire::Bytes(value), .. } if *found == number => Some(value.as_slice()),
        _ => None,
    })
}

pub(crate) fn all_bytes<'a>(fields: &'a [Field], number: u32) -> Vec<&'a [u8]> {
    fields.iter().filter_map(|field| match field {
        Field { number: found, value: Wire::Bytes(value), .. } if *found == number => Some(value.as_slice()),
        _ => None,
    }).collect()
}

pub(crate) fn text(fields: &[Field], number: u32) -> Result<Option<String>> {
    bytes(fields, number).map(utf8).transpose()
}

pub(crate) fn message(fields: &[Field], number: u32) -> Result<Vec<Field>> {
    match bytes(fields, number) {
        Some(value) => decode(value),
        None => Ok(Vec::new()),
    }
}

fn utf8(bytes: &[u8]) -> Result<String> {
    std::str::from_utf8(bytes).map(str::to_owned).map_err(|_| anyhow::anyhow!("protobuf string is not UTF-8"))
}

/// Replace every occurrence of one length-delimited field, keeping every other field.
pub(crate) fn replace_bytes(fields: &[Field], number: u32, values: &[Vec<u8>]) -> Vec<Field> {
    let mut replaced = Vec::with_capacity(fields.len() + values.len());
    let mut inserted = false;
    for field in fields {
        if field.number == number {
            if !inserted {
                replaced.extend(values.iter().cloned().map(|value| Field { number, value: Wire::Bytes(value) }));
                inserted = true;
            }
        } else {
            replaced.push(field.clone());
        }
    }
    if !inserted {
        replaced.extend(values.iter().cloned().map(|value| Field { number, value: Wire::Bytes(value) }));
    }
    replaced
}

pub(crate) const BASE64_PREFIX: &str = "base64:";

pub(crate) fn base64_encode(bytes: &[u8]) -> String {
    const ALPHABET: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";
    let mut out = String::with_capacity(bytes.len().div_ceil(3) * 4);
    for chunk in bytes.chunks(3) {
        let mut value = 0_u32;
        for (index, byte) in chunk.iter().enumerate() {
            value |= u32::from(*byte) << (16 - 8 * index);
        }
        for index in 0..=chunk.len() {
            out.push(char::from(ALPHABET[((value >> (18 - 6 * index)) & 0x3f) as usize]));
        }
        for _ in chunk.len()..3 {
            out.push('=');
        }
    }
    out
}

pub(crate) fn base64_decode(text: &str) -> Result<Vec<u8>> {
    if text.len() % 4 != 0 || text.bytes().any(|byte| !matches!(byte, b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'+' | b'/' | b'=')) {
        bail!("invalid base64 image or signature");
    }
    let mut out = Vec::with_capacity(text.len() / 4 * 3);
    for chunk in text.as_bytes().chunks(4) {
        let padded = chunk.iter().rev().take_while(|byte| **byte == b'=').count();
        if padded > 2 || chunk[..4 - padded].contains(&b'=') {
            bail!("invalid base64 padding");
        }
        let mut value = 0_u32;
        for byte in &chunk[..4 - padded] {
            let digit = match byte {
                b'A'..=b'Z' => byte - b'A',
                b'a'..=b'z' => byte - b'a' + 26,
                b'0'..=b'9' => byte - b'0' + 52,
                b'+' => 62,
                b'/' => 63,
                _ => unreachable!("alphabet checked"),
            };
            value = (value << 6) | u32::from(digit);
        }
        value <<= 6 * padded;
        out.extend(value.to_be_bytes()[1..4 - padded].iter().copied());
    }
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn round_trips_mixed_fields_and_rejects_truncation() {
        let mut bytes = Vec::new();
        varint_field(&mut bytes, 1, 300);
        string_field(&mut bytes, 2, "hello");
        bytes_field(&mut bytes, 5, &[0xff, 0xfe]);
        let fields = decode(&bytes).expect("decode");
        assert_eq!(varint(&fields, 1), Some(300));
        assert_eq!(text(&fields, 2).unwrap().as_deref(), Some("hello"));
        assert_eq!(encode(&fields), bytes);
        bytes.pop();
        assert!(decode(&bytes).is_err());
    }

    #[test]
    fn base64_round_trip_covers_padding() {
        for value in [b"".as_slice(), b"a", b"ab", b"abc", &[0, 255, 10]] {
            let encoded = base64_encode(value);
            assert_eq!(base64_decode(&encoded).unwrap(), value);
        }
        assert!(base64_decode("@@@=").is_err());
    }
}
