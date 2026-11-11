//! rclone-compatible encryption ("crypt" remote, standard name encryption, base32 names).
//!
//! Everything Drive uploads is encrypted in exactly the format `rclone crypt` uses, so the
//! data stays readable with stock rclone if Drive ever disappears (see `recovery`).
//!
//! * Keys: scrypt(password, salt, N=16384, r=8, p=1) → 32 B data key, 32 B name key, 16 B tweak.
//! * Names: PKCS#7 pad → AES-256-EME(name key, tweak) → lowercase base32hex without padding.
//! * Content: `RCLONE\0\0` + 24 B random nonce, then 64 KiB chunks sealed with NaCl secretbox
//!   (XSalsa20-Poly1305); the nonce is incremented (little-endian) for every chunk.

pub mod eme;
pub mod obscure;

use aes::Aes256;

use crypto_secretbox::XSalsa20Poly1305;
use crypto_secretbox::aead::{AeadInPlace, KeyInit};
use rand::RngCore;
use zeroize::Zeroize;

pub const FILE_MAGIC: &[u8; 8] = b"RCLONE\x00\x00";
pub const NONCE_SIZE: usize = 24;
pub const HEADER_SIZE: usize = FILE_MAGIC.len() + NONCE_SIZE;
pub const BLOCK_HEADER_SIZE: usize = 16;
pub const BLOCK_DATA_SIZE: usize = 64 * 1024;
pub const BLOCK_SIZE: usize = BLOCK_HEADER_SIZE + BLOCK_DATA_SIZE;

/// rclone's built-in salt, used when no salt (`password2`) is configured.
pub const DEFAULT_SALT: [u8; 16] = [
    0xA8, 0x0D, 0xF4, 0x3A, 0x8F, 0xBD, 0x03, 0x08, 0xA7, 0xCA, 0xB8, 0x3E, 0x58, 0x1F, 0x86, 0xB1,
];

#[derive(Debug, thiserror::Error, PartialEq, Eq)]
pub enum CryptError {
    #[error("bad base32 file name encoding")]
    BadBase32,
    #[error("encrypted name is not a multiple of the block size")]
    NotMultipleOfBlockSize,
    #[error("encrypted name too short")]
    TooShort,
    #[error("encrypted name too long")]
    TooLong,
    #[error("bad padding in encrypted name")]
    BadPadding,
    #[error("decrypted name is not valid UTF-8")]
    BadUtf8,
    #[error("name is too long to encrypt")]
    NameTooLong,
    #[error("not an encrypted file: bad magic")]
    BadMagic,
    #[error("encrypted file is too short")]
    FileTooShort,
    #[error("encrypted file has a truncated block")]
    TruncatedBlock,
    #[error("failed to authenticate a block (wrong password or corrupted data)")]
    BadBlock,
    #[error("key derivation failed")]
    Kdf,
}

/// The 24-byte per-file nonce. Each 64 KiB block uses `file_nonce + block_index`.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Nonce(pub [u8; NONCE_SIZE]);

impl Nonce {
    pub fn random() -> Self {
        let mut n = [0u8; NONCE_SIZE];
        rand::thread_rng().fill_bytes(&mut n);
        Nonce(n)
    }

    fn carry(&mut self, mut i: usize) {
        while i < NONCE_SIZE {
            let digit = self.0[i];
            let new = digit.wrapping_add(1);
            self.0[i] = new;
            if new >= digit {
                break;
            }
            i += 1;
        }
    }

    pub fn increment(&mut self) {
        self.carry(0);
    }

    /// Adds `x` (little-endian) — used to jump straight to block `x` when seeking.
    pub fn add(&mut self, mut x: u64) {
        let mut carry: u16 = 0;
        for i in 0..8 {
            let digit = self.0[i] as u16;
            let x_digit = (x & 0xff) as u16;
            x >>= 8;
            carry += digit + x_digit;
            self.0[i] = carry as u8;
            carry >>= 8;
        }
        if carry != 0 {
            self.carry(8);
        }
    }

    pub fn plus(mut self, x: u64) -> Self {
        self.add(x);
        self
    }
}

/// Derived keys for one crypt configuration. Cheap to clone; keys are wiped on drop.
#[derive(Clone)]
pub struct Cipher {
    data_key: [u8; 32],
    name_key: [u8; 32],
    name_tweak: [u8; 16],
    name_block: Aes256,
    box_cipher: XSalsa20Poly1305,
}

impl Drop for Cipher {
    fn drop(&mut self) {
        self.data_key.zeroize();
        self.name_key.zeroize();
        self.name_tweak.zeroize();
    }
}

impl std::fmt::Debug for Cipher {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("Cipher(..)")
    }
}

impl Cipher {
    /// Derives keys exactly like rclone: an empty salt means rclone's default salt, and an
    /// empty password yields all-zero keys (only ever used by tests).
    pub fn new(password: &str, salt: &str) -> Result<Self, CryptError> {
        let mut key = [0u8; 80];
        if !password.is_empty() {
            let salt_bytes: &[u8] = if salt.is_empty() { &DEFAULT_SALT } else { salt.as_bytes() };
            let params = scrypt::Params::new(14, 8, 1, scrypt::Params::RECOMMENDED_LEN).map_err(|_| CryptError::Kdf)?;
            scrypt::scrypt(password.as_bytes(), salt_bytes, &params, &mut key)
                .map_err(|_| CryptError::Kdf)?;
        }
        let mut data_key = [0u8; 32];
        let mut name_key = [0u8; 32];
        let mut name_tweak = [0u8; 16];
        data_key.copy_from_slice(&key[..32]);
        name_key.copy_from_slice(&key[32..64]);
        name_tweak.copy_from_slice(&key[64..80]);
        key.zeroize();
        let name_block = Aes256::new(aes::cipher::generic_array::GenericArray::from_slice(&name_key));
        let box_cipher = XSalsa20Poly1305::new(&data_key.into());
        Ok(Self { data_key, name_key, name_tweak, name_block, box_cipher })
    }

    // ---- names -------------------------------------------------------------------------

    /// Encrypts one path segment (a file or folder name).
    pub fn encrypt_name(&self, plain: &str) -> Result<String, CryptError> {
        if plain.is_empty() {
            return Ok(String::new());
        }
        let bytes = plain.as_bytes();
        let pad = 16 - bytes.len() % 16;
        let mut padded = Vec::with_capacity(bytes.len() + pad);
        padded.extend_from_slice(bytes);
        padded.extend(std::iter::repeat_n(pad as u8, pad));
        if padded.len() / 16 > eme::MAX_BLOCKS {
            return Err(CryptError::NameTooLong);
        }
        let ct = eme::transform(&self.name_block, &self.name_tweak, &padded, eme::Direction::Encrypt);
        Ok(base32_encode(&ct))
    }

    /// Decrypts one path segment.
    pub fn decrypt_name(&self, encrypted: &str) -> Result<String, CryptError> {
        if encrypted.is_empty() {
            return Ok(String::new());
        }
        let raw = base32_decode(encrypted)?;
        if raw.len() % 16 != 0 {
            return Err(CryptError::NotMultipleOfBlockSize);
        }
        if raw.is_empty() {
            return Err(CryptError::TooShort);
        }
        if raw.len() > 2048 {
            return Err(CryptError::TooLong);
        }
        let padded = eme::transform(&self.name_block, &self.name_tweak, &raw, eme::Direction::Decrypt);
        let pad = *padded.last().ok_or(CryptError::BadPadding)? as usize;
        if pad == 0 || pad > 16 || pad > padded.len() {
            return Err(CryptError::BadPadding);
        }
        if padded[padded.len() - pad..].iter().any(|&b| b as usize != pad) {
            return Err(CryptError::BadPadding);
        }
        String::from_utf8(padded[..padded.len() - pad].to_vec()).map_err(|_| CryptError::BadUtf8)
    }

    /// Encrypts a `/`-separated relative path segment by segment (directory names included).
    pub fn encrypt_path(&self, path: &str) -> Result<String, CryptError> {
        path.split('/').map(|s| self.encrypt_name(s)).collect::<Result<Vec<_>, _>>().map(|v| v.join("/"))
    }

    pub fn decrypt_path(&self, path: &str) -> Result<String, CryptError> {
        path.split('/').map(|s| self.decrypt_name(s)).collect::<Result<Vec<_>, _>>().map(|v| v.join("/"))
    }

    // ---- content -----------------------------------------------------------------------

    /// Seals one plaintext block (at most 64 KiB) into `out` as `tag || ciphertext`.
    pub fn seal_block(&self, nonce: &Nonce, plain: &[u8], out: &mut Vec<u8>) {
        debug_assert!(plain.len() <= BLOCK_DATA_SIZE);
        let start = out.len();
        out.extend_from_slice(&[0u8; BLOCK_HEADER_SIZE]);
        out.extend_from_slice(plain);
        let tag = self
            .box_cipher
            .encrypt_in_place_detached(&nonce.0.into(), b"", &mut out[start + BLOCK_HEADER_SIZE..])
            .expect("secretbox encryption cannot fail for in-range block sizes");
        out[start..start + BLOCK_HEADER_SIZE].copy_from_slice(&tag);
    }

    /// Opens one sealed block (`tag || ciphertext`), appending the plaintext to `out`.
    pub fn open_block(&self, nonce: &Nonce, sealed: &[u8], out: &mut Vec<u8>) -> Result<(), CryptError> {
        if sealed.len() <= BLOCK_HEADER_SIZE {
            return Err(CryptError::TruncatedBlock);
        }
        let (tag, body) = sealed.split_at(BLOCK_HEADER_SIZE);
        let start = out.len();
        out.extend_from_slice(body);
        if self
            .box_cipher
            .decrypt_in_place_detached(&nonce.0.into(), b"", &mut out[start..], tag.into())
            .is_err()
        {
            out.truncate(start);
            return Err(CryptError::BadBlock);
        }
        Ok(())
    }

    pub fn encryptor(&self) -> Encryptor {
        Encryptor::new(self.clone(), Nonce::random())
    }

    pub fn decryptor(&self) -> Decryptor {
        Decryptor::from_start(self.clone())
    }
}

/// Size of the encrypted object for a plaintext of `size` bytes.
pub fn encrypted_size(size: u64) -> u64 {
    let blocks = size / BLOCK_DATA_SIZE as u64;
    let residue = size % BLOCK_DATA_SIZE as u64;
    let mut out = HEADER_SIZE as u64 + blocks * BLOCK_SIZE as u64;
    if residue != 0 {
        out += BLOCK_HEADER_SIZE as u64 + residue;
    }
    out
}

/// Plaintext size of an encrypted object of `size` bytes.
pub fn decrypted_size(size: u64) -> Result<u64, CryptError> {
    let size = size.checked_sub(HEADER_SIZE as u64).ok_or(CryptError::FileTooShort)?;
    let blocks = size / BLOCK_SIZE as u64;
    let mut residue = size % BLOCK_SIZE as u64;
    if residue != 0 {
        residue = residue.checked_sub(BLOCK_HEADER_SIZE as u64).filter(|r| *r > 0).ok_or(CryptError::TruncatedBlock)?;
    }
    Ok(blocks * BLOCK_DATA_SIZE as u64 + residue)
}

/// Where to read in the encrypted object to serve plaintext `[offset, offset+len)`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct UnderlyingRange {
    /// Index of the first block needed.
    pub first_block: u64,
    /// Byte offset of that block in the encrypted object.
    pub offset: u64,
    /// Number of encrypted bytes to read (whole blocks; the last may be short at EOF).
    pub len: u64,
    /// Plaintext bytes to drop from the start of the first decrypted block.
    pub discard: u64,
}

pub fn underlying_range(offset: u64, len: u64) -> UnderlyingRange {
    let bds = BLOCK_DATA_SIZE as u64;
    let first_block = offset / bds;
    let discard = offset % bds;
    let last_byte = offset + len.max(1) - 1;
    let last_block = last_byte / bds;
    let blocks = last_block - first_block + 1;
    UnderlyingRange {
        first_block,
        offset: HEADER_SIZE as u64 + first_block * BLOCK_SIZE as u64,
        len: blocks * BLOCK_SIZE as u64,
        discard,
    }
}

/// Streaming encryptor: emit [`Encryptor::header`] first, then each ≤64 KiB plaintext chunk.
pub struct Encryptor {
    cipher: Cipher,
    nonce: Nonce,
    header: [u8; HEADER_SIZE],
}

impl Encryptor {
    pub fn new(cipher: Cipher, nonce: Nonce) -> Self {
        let mut header = [0u8; HEADER_SIZE];
        header[..8].copy_from_slice(FILE_MAGIC);
        header[8..].copy_from_slice(&nonce.0);
        Self { cipher, nonce, header }
    }

    pub fn header(&self) -> &[u8; HEADER_SIZE] {
        &self.header
    }

    /// Encrypts one block of plaintext. Every block except the last must be exactly 64 KiB.
    pub fn encrypt_block(&mut self, plain: &[u8], out: &mut Vec<u8>) {
        self.cipher.seal_block(&self.nonce, plain, out);
        self.nonce.increment();
    }
}

/// Push-based decryptor. Feed encrypted bytes in any chunking; plaintext comes out in order.
pub struct Decryptor {
    cipher: Cipher,
    nonce: Option<Nonce>,
    pending: Vec<u8>,
}

impl Decryptor {
    /// Decrypts an object read from its very beginning (header included).
    pub fn from_start(cipher: Cipher) -> Self {
        Self { cipher, nonce: None, pending: Vec::with_capacity(BLOCK_SIZE) }
    }

    /// Decrypts an object read from the start of block `block_index` (no header in the stream).
    pub fn at_block(cipher: Cipher, file_nonce: Nonce, block_index: u64) -> Self {
        Self { cipher, nonce: Some(file_nonce.plus(block_index)), pending: Vec::with_capacity(BLOCK_SIZE) }
    }

    /// Parses an object header, returning the file nonce.
    pub fn parse_header(header: &[u8]) -> Result<Nonce, CryptError> {
        if header.len() < HEADER_SIZE {
            return Err(CryptError::FileTooShort);
        }
        if &header[..8] != FILE_MAGIC {
            return Err(CryptError::BadMagic);
        }
        let mut n = [0u8; NONCE_SIZE];
        n.copy_from_slice(&header[8..HEADER_SIZE]);
        Ok(Nonce(n))
    }

    pub fn feed(&mut self, mut data: &[u8], out: &mut Vec<u8>) -> Result<(), CryptError> {
        if self.nonce.is_none() {
            let need = HEADER_SIZE - self.pending.len();
            let take = need.min(data.len());
            self.pending.extend_from_slice(&data[..take]);
            data = &data[take..];
            if self.pending.len() < HEADER_SIZE {
                return Ok(());
            }
            self.nonce = Some(Self::parse_header(&self.pending)?);
            self.pending.clear();
        }
        while !data.is_empty() {
            let need = BLOCK_SIZE - self.pending.len();
            if self.pending.is_empty() && data.len() >= BLOCK_SIZE {
                // Fast path: decrypt directly from the input slice.
                let nonce = self.nonce.as_mut().expect("nonce set");
                self.cipher.open_block(nonce, &data[..BLOCK_SIZE], out)?;
                nonce.increment();
                data = &data[BLOCK_SIZE..];
                continue;
            }
            let take = need.min(data.len());
            self.pending.extend_from_slice(&data[..take]);
            data = &data[take..];
            if self.pending.len() == BLOCK_SIZE {
                let nonce = self.nonce.as_mut().expect("nonce set");
                self.cipher.open_block(nonce, &self.pending, out)?;
                nonce.increment();
                self.pending.clear();
            }
        }
        Ok(())
    }

    /// Flushes the final (short) block. Errors if the stream ended mid-header.
    pub fn finish(&mut self, out: &mut Vec<u8>) -> Result<(), CryptError> {
        if self.nonce.is_none() {
            return Err(CryptError::FileTooShort);
        }
        if !self.pending.is_empty() {
            let nonce = self.nonce.as_mut().expect("nonce set");
            self.cipher.open_block(nonce, &self.pending, out)?;
            nonce.increment();
            self.pending.clear();
        }
        Ok(())
    }
}

/// Encrypts a whole buffer (tests and small files).
pub fn encrypt_bytes(cipher: &Cipher, nonce: Nonce, plain: &[u8]) -> Vec<u8> {
    let mut enc = Encryptor::new(cipher.clone(), nonce);
    let mut out = Vec::with_capacity(encrypted_size(plain.len() as u64) as usize);
    out.extend_from_slice(enc.header());
    for chunk in plain.chunks(BLOCK_DATA_SIZE) {
        enc.encrypt_block(chunk, &mut out);
    }
    out
}

/// Decrypts a whole buffer (tests and small files).
pub fn decrypt_bytes(cipher: &Cipher, data: &[u8]) -> Result<Vec<u8>, CryptError> {
    let mut d = Decryptor::from_start(cipher.clone());
    let mut out = Vec::with_capacity(data.len());
    d.feed(data, &mut out)?;
    d.finish(&mut out)?;
    Ok(out)
}

fn base32_encode(data: &[u8]) -> String {
    data_encoding::BASE32HEX_NOPAD.encode(data).to_ascii_lowercase()
}

fn base32_decode(s: &str) -> Result<Vec<u8>, CryptError> {
    if s.ends_with('=') {
        return Err(CryptError::BadBase32);
    }
    data_encoding::BASE32HEX_NOPAD.decode(s.to_ascii_uppercase().as_bytes()).map_err(|_| CryptError::BadBase32)
}

/// Generates a strong random secret suitable for a crypt password or salt.
pub fn generate_secret() -> String {
    let mut bytes = [0u8; 32];
    rand::thread_rng().fill_bytes(&mut bytes);
    data_encoding::BASE64URL_NOPAD.encode(&bytes)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn zero_cipher() -> Cipher {
        Cipher::new("", "").unwrap()
    }

    // Vectors from rclone backend/crypt/cipher_test.go (empty password => zero keys).
    #[test]
    fn rclone_name_vectors() {
        let c = zero_cipher();
        let cases = [
            ("1", "p0e52nreeaj0a5ea7s64m4j72s"),
            ("1/12", "p0e52nreeaj0a5ea7s64m4j72s/l42g6771hnv3an9cgc8cr2n1ng"),
            (
                "1/12/123",
                "p0e52nreeaj0a5ea7s64m4j72s/l42g6771hnv3an9cgc8cr2n1ng/qgm4avr35m5loi1th53ato71v0",
            ),
        ];
        for (plain, enc) in cases {
            assert_eq!(c.encrypt_path(plain).unwrap(), enc, "encrypt {plain}");
            assert_eq!(c.decrypt_path(enc).unwrap(), plain, "decrypt {enc}");
        }
    }

    #[test]
    fn name_roundtrip_with_password() {
        let c = Cipher::new("correct horse", "battery staple").unwrap();
        for name in ["a", "Ünïcødé ファイル.txt", &"x".repeat(143)] {
            let enc = c.encrypt_name(name).unwrap();
            assert_eq!(c.decrypt_name(&enc).unwrap(), name);
        }
        assert!(c.decrypt_name("not-valid!").is_err());
    }

    #[test]
    fn sizes() {
        for size in [0u64, 1, 65535, 65536, 65537, 10 * 65536 + 7] {
            let e = encrypted_size(size);
            assert_eq!(decrypted_size(e).unwrap(), size);
        }
        assert_eq!(encrypted_size(0), 32);
        assert_eq!(encrypted_size(1), 32 + 17);
    }

    #[test]
    fn content_roundtrip_and_seek() {
        let c = Cipher::new("pw", "salt").unwrap();
        let plain: Vec<u8> = (0..(3 * BLOCK_DATA_SIZE + 1234)).map(|i| (i * 31 % 251) as u8).collect();
        let nonce = Nonce::random();
        let enc = encrypt_bytes(&c, nonce, &plain);
        assert_eq!(enc.len() as u64, encrypted_size(plain.len() as u64));
        assert_eq!(decrypt_bytes(&c, &enc).unwrap(), plain);

        // Odd chunking.
        let mut d = c.decryptor();
        let mut out = Vec::new();
        for chunk in enc.chunks(7777) {
            d.feed(chunk, &mut out).unwrap();
        }
        d.finish(&mut out).unwrap();
        assert_eq!(out, plain);

        // Seek into the middle.
        let (off, len) = (BLOCK_DATA_SIZE as u64 + 100, 70_000u64);
        let r = underlying_range(off, len);
        let end = ((r.offset + r.len) as usize).min(enc.len());
        let file_nonce = Decryptor::parse_header(&enc).unwrap();
        let mut d = Decryptor::at_block(c.clone(), file_nonce, r.first_block);
        let mut out = Vec::new();
        d.feed(&enc[r.offset as usize..end], &mut out).unwrap();
        d.finish(&mut out).unwrap();
        let got = &out[r.discard as usize..r.discard as usize + len as usize];
        assert_eq!(got, &plain[off as usize..(off + len) as usize]);
    }

    #[test]
    fn tamper_detected() {
        let c = Cipher::new("pw", "").unwrap();
        let mut enc = encrypt_bytes(&c, Nonce::random(), b"hello world");
        let last = enc.len() - 1;
        enc[last] ^= 1;
        assert_eq!(decrypt_bytes(&c, &enc), Err(CryptError::BadBlock));
        let wrong = Cipher::new("other", "").unwrap();
        let enc = encrypt_bytes(&c, Nonce::random(), b"hello world");
        assert_eq!(decrypt_bytes(&wrong, &enc), Err(CryptError::BadBlock));
    }

    #[test]
    fn nonce_add_matches_increment() {
        let mut a = Nonce([0xff; 24]);
        a.0[10] = 3;
        let mut b = a;
        for _ in 0..300 {
            a.increment();
        }
        b.add(300);
        assert_eq!(a, b);
    }

    #[test]
    fn empty_file() {
        let c = Cipher::new("pw", "").unwrap();
        let enc = encrypt_bytes(&c, Nonce::random(), b"");
        assert_eq!(enc.len(), HEADER_SIZE);
        assert_eq!(decrypt_bytes(&c, &enc).unwrap(), Vec::<u8>::new());
    }
}
