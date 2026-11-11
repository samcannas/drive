//! The file metadata rclone stores in a uloz.to file's `description` field.
//!
//! uloz.to cannot store a modification time or hashes, so rclone serialises
//! `struct { Md5Hash []byte; Sha256Hash []byte; ModTimeEpochMicros int64 }` with Go's `gob`
//! encoding, base64s it and prefixes `1;`. We read and write the identical format so files
//! uploaded by Drive look right to `rclone` and vice versa.
//!
//! Only this one struct is supported: the type-definition message is a constant (Go always
//! emits the same bytes for it), and the value message is a tiny hand-rolled encoder.

use base64::Engine;
use base64::engine::general_purpose::STANDARD;

/// Go's gob type definition for rclone's `DescriptionEncodedMetadata`, length prefix included.
const TYPE_DEF: &[u8] = &[
    0x59, 0x7f, 0x03, 0x01, 0x01, 0x1a, b'D', b'e', b's', b'c', b'r', b'i', b'p', b't', b'i', b'o',
    b'n', b'E', b'n', b'c', b'o', b'd', b'e', b'd', b'M', b'e', b't', b'a', b'd', b'a', b't', b'a',
    0x01, 0xff, 0x80, 0x00, 0x01, 0x03, 0x01, 0x07, b'M', b'd', b'5', b'H', b'a', b's', b'h', 0x01,
    0x0a, 0x00, 0x01, 0x0a, b'S', b'h', b'a', b'2', b'5', b'6', b'H', b'a', b's', b'h', 0x01, 0x0a,
    0x00, 0x01, 0x12, b'M', b'o', b'd', b'T', b'i', b'm', b'e', b'E', b'p', b'o', b'c', b'h', b'M',
    b'i', b'c', b'r', b'o', b's', 0x01, 0x04, 0x00, 0x00, 0x00,
];

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct RcloneMeta {
    /// MD5 of the stored (possibly encrypted) bytes.
    pub md5: Vec<u8>,
    /// SHA-256 of the stored (possibly encrypted) bytes.
    pub sha256: Vec<u8>,
    /// Modification time in microseconds since the Unix epoch.
    pub mtime_micros: i64,
}

fn put_uint(out: &mut Vec<u8>, v: u64) {
    if v < 128 {
        out.push(v as u8);
        return;
    }
    let bytes = v.to_be_bytes();
    let skip = bytes.iter().take_while(|&&b| b == 0).count();
    let n = 8 - skip;
    out.push((256 - n) as u8);
    out.extend_from_slice(&bytes[skip..]);
}

fn put_int(out: &mut Vec<u8>, v: i64) {
    let u = if v < 0 {
        ((!v as u64) << 1) | 1
    } else {
        (v as u64) << 1
    };
    put_uint(out, u);
}

fn get_uint(buf: &[u8], pos: &mut usize) -> Option<u64> {
    let b = *buf.get(*pos)?;
    *pos += 1;
    if b < 128 {
        return Some(b as u64);
    }
    let n = 256 - b as usize;
    if n > 8 || *pos + n > buf.len() {
        return None;
    }
    let mut v = 0u64;
    for &x in &buf[*pos..*pos + n] {
        v = (v << 8) | x as u64;
    }
    *pos += n;
    Some(v)
}

fn get_int(buf: &[u8], pos: &mut usize) -> Option<i64> {
    let u = get_uint(buf, pos)?;
    Some(if u & 1 == 1 {
        !((u >> 1) as i64)
    } else {
        (u >> 1) as i64
    })
}

/// Encodes metadata as an rclone-compatible description string (`1;<base64 gob>`).
pub fn encode(meta: &RcloneMeta) -> String {
    let mut value = Vec::new();
    put_int(&mut value, 64); // type id of the struct defined in TYPE_DEF
    let mut last_field: i64 = -1;
    let mut field = |value: &mut Vec<u8>, idx: i64| {
        put_uint(value, (idx - last_field) as u64);
        last_field = idx;
    };
    if !meta.md5.is_empty() {
        field(&mut value, 0);
        put_uint(&mut value, meta.md5.len() as u64);
        value.extend_from_slice(&meta.md5);
    }
    if !meta.sha256.is_empty() {
        field(&mut value, 1);
        put_uint(&mut value, meta.sha256.len() as u64);
        value.extend_from_slice(&meta.sha256);
    }
    if meta.mtime_micros != 0 {
        field(&mut value, 2);
        put_int(&mut value, meta.mtime_micros);
    }
    value.push(0); // end of struct

    let mut msg = TYPE_DEF.to_vec();
    put_uint(&mut msg, value.len() as u64);
    msg.extend_from_slice(&value);
    format!("1;{}", STANDARD.encode(msg))
}

/// Decodes an rclone description. Returns `None` for anything else (e.g. a user's own text).
pub fn decode(description: &str) -> Option<RcloneMeta> {
    let payload = description.strip_prefix("1;")?;
    let buf = STANDARD.decode(payload.trim()).ok()?;
    let mut pos = 0;
    while pos < buf.len() {
        let len = get_uint(&buf, &mut pos)? as usize;
        let end = pos.checked_add(len)?;
        if end > buf.len() {
            return None;
        }
        let msg = &buf[pos..end];
        pos = end;
        let mut p = 0;
        let type_id = get_int(msg, &mut p)?;
        if type_id < 0 {
            continue; // type definition
        }
        let mut meta = RcloneMeta::default();
        let mut field: i64 = -1;
        loop {
            let delta = get_uint(msg, &mut p)?;
            if delta == 0 {
                return Some(meta);
            }
            field += delta as i64;
            match field {
                0 | 1 => {
                    let n = get_uint(msg, &mut p)? as usize;
                    let bytes = msg.get(p..p + n)?.to_vec();
                    p += n;
                    if field == 0 {
                        meta.md5 = bytes;
                    } else {
                        meta.sha256 = bytes;
                    }
                }
                2 => meta.mtime_micros = get_int(msg, &mut p)?,
                _ => return None,
            }
        }
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;

    // Produced by Go's encoding/gob with rclone's struct (see docs/development.md).
    const FULL: &str = "1;WX8DAQEaRGVzY3JpcHRpb25FbmNvZGVkTWV0YWRhdGEB/4AAAQMBB01kNUhhc2gBCgABClNoYTI1Nkhhc2gBCgABEk1vZFRpbWVFcG9jaE1pY3JvcwEEAAAAQP+AARBdQUAqvEsqdrlxnZEQF8WSASAs8k26X7CjDiboOyrFueKeGxYeXB+nQl5zBDNik4uYJAH5DE0Y6srkAAA=";
    const NEG: &str = "1;WX8DAQEaRGVzY3JpcHRpb25FbmNvZGVkTWV0YWRhdGEB/4AAAQMBB01kNUhhc2gBCgABClNoYTI1Nkhhc2gBCgABEk1vZFRpbWVFcG9jaE1pY3JvcwEEAAAABf+AAwkA";
    const EMPTY: &str = "1;WX8DAQEaRGVzY3JpcHRpb25FbmNvZGVkTWV0YWRhdGEB/4AAAQMBB01kNUhhc2gBCgABClNoYTI1Nkhhc2gBCgABEk1vZFRpbWVFcG9jaE1pY3JvcwEEAAAAA/+AAA==";
    const SKIP: &str = "1;WX8DAQEaRGVzY3JpcHRpb25FbmNvZGVkTWV0YWRhdGEB/4AAAQMBB01kNUhhc2gBCgABClNoYTI1Nkhhc2gBCgABEk1vZFRpbWVFcG9jaE1pY3JvcwEEAAAAGf+AARBdQUAqvEsqdrlxnZEQF8WSAv4CWAA=";

    fn md5() -> Vec<u8> {
        hex::decode("5d41402abc4b2a76b9719d911017c592").unwrap()
    }
    fn sha() -> Vec<u8> {
        hex::decode("2cf24dba5fb0a30e26e83b2ac5b9e29e1b161e5c1fa7425e73043362938b9824").unwrap()
    }

    #[test]
    fn matches_go_encoder() {
        let full = RcloneMeta {
            md5: md5(),
            sha256: sha(),
            mtime_micros: 1731234567123456,
        };
        assert_eq!(encode(&full), FULL);
        assert_eq!(
            encode(&RcloneMeta {
                mtime_micros: -5,
                ..Default::default()
            }),
            NEG
        );
        assert_eq!(encode(&RcloneMeta::default()), EMPTY);
        assert_eq!(
            encode(&RcloneMeta {
                md5: md5(),
                sha256: vec![],
                mtime_micros: 300
            }),
            SKIP
        );
    }

    #[test]
    fn decodes_go_output() {
        assert_eq!(
            decode(FULL).unwrap(),
            RcloneMeta {
                md5: md5(),
                sha256: sha(),
                mtime_micros: 1731234567123456
            }
        );
        assert_eq!(decode(NEG).unwrap().mtime_micros, -5);
        assert_eq!(decode(EMPTY).unwrap(), RcloneMeta::default());
        assert_eq!(
            decode(SKIP).unwrap(),
            RcloneMeta {
                md5: md5(),
                sha256: vec![],
                mtime_micros: 300
            }
        );
        assert!(decode("my holiday photos").is_none());
        assert!(decode("1;!!!").is_none());
    }
}
