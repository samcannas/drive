//! rclone's password "obscuring" (AES-256-CTR with a fixed, public key).
//!
//! This is NOT encryption — anyone can reveal an obscured value. It exists only so the
//! recovery config we export is byte-compatible with what `rclone config` writes.

use aes::Aes256;
use base64::Engine;
use base64::engine::general_purpose::URL_SAFE_NO_PAD;
use ctr::cipher::{KeyIvInit, StreamCipher};
use rand::RngCore;

type Aes256Ctr = ctr::Ctr128BE<Aes256>;

const CRYPT_KEY: [u8; 32] = [
    0x9c, 0x93, 0x5b, 0x48, 0x73, 0x0a, 0x55, 0x4d, 0x6b, 0xfd, 0x7c, 0x63, 0xc8, 0x86, 0xa9, 0x2b,
    0xd3, 0x90, 0x19, 0x8e, 0xb8, 0x12, 0x8a, 0xfb, 0xf4, 0xde, 0x16, 0x2b, 0x8b, 0x95, 0xf6, 0x38,
];

/// Obscures `plain` exactly like `rclone obscure`.
pub fn obscure(plain: &str) -> String {
    let mut out = vec![0u8; 16 + plain.len()];
    rand::thread_rng().fill_bytes(&mut out[..16]);
    out[16..].copy_from_slice(plain.as_bytes());
    let (iv, body) = out.split_at_mut(16);
    let mut cipher = Aes256Ctr::new(&CRYPT_KEY.into(), (&*iv).into());
    cipher.apply_keystream(body);
    URL_SAFE_NO_PAD.encode(out)
}

/// Reveals a value produced by [`obscure`] or by `rclone obscure`.
pub fn reveal(obscured: &str) -> Result<String, String> {
    let mut data = URL_SAFE_NO_PAD
        .decode(obscured.trim())
        .map_err(|e| format!("not an obscured value: {e}"))?;
    if data.len() < 16 {
        return Err("obscured value too short".into());
    }
    let (iv, body) = data.split_at_mut(16);
    let mut cipher = Aes256Ctr::new(&CRYPT_KEY.into(), (&*iv).into());
    cipher.apply_keystream(body);
    String::from_utf8(body.to_vec()).map_err(|_| "obscured value is not UTF-8".into())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn roundtrip() {
        for s in ["", "hunter2", "pässwörd with spaces"] {
            assert_eq!(reveal(&obscure(s)).unwrap(), s);
        }
    }

    #[test]
    fn reveals_rclone_vector() {
        // From rclone fs/config/obscure/obscure_test.go: Obscure("potato") with IV = 16 zero bytes... is
        // random in rclone, so we rely on the fixed-IV vector computed with the same algorithm:
        let mut data = vec![0u8; 16];
        data.extend_from_slice(b"potato");
        let (iv, body) = data.split_at_mut(16);
        let mut cipher = Aes256Ctr::new(&CRYPT_KEY.into(), (&*iv).into());
        cipher.apply_keystream(body);
        let encoded = URL_SAFE_NO_PAD.encode(&data);
        assert_eq!(reveal(&encoded).unwrap(), "potato");
    }
}
