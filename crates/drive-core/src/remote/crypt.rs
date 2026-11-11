//! A [`Remote`] that encrypts names and contents with rclone's crypt format before handing
//! them to an inner remote, and decrypts on the way back.

use std::collections::HashMap;
use std::sync::{Arc, Mutex};

use async_trait::async_trait;
use bytes::{Bytes, BytesMut};
use futures::StreamExt;

use super::{ByteStream, Kind, Remote, RemoteEntry, RemoteError, RemoteTree, UploadBody};
use crate::crypt::{self, BLOCK_DATA_SIZE, Cipher, CryptError, Decryptor, Encryptor, HEADER_SIZE, Nonce};

/// Max plaintext name length that still fits uloz.to's 255-character limit once encrypted.
/// 143 bytes → 144 padded → 9 blocks → 144 bytes → 231 base32 chars; 159 would be 256.
pub const MAX_NAME_BYTES: usize = 143;

pub struct CryptRemote<R: Remote> {
    inner: Arc<R>,
    cipher: Cipher,
    /// File nonces we have already read, so seeks need only one request.
    nonces: Mutex<HashMap<String, Nonce>>,
}

impl<R: Remote> CryptRemote<R> {
    pub fn new(inner: Arc<R>, cipher: Cipher) -> Self {
        Self { inner, cipher, nonces: Mutex::new(HashMap::new()) }
    }

    pub fn inner(&self) -> &Arc<R> {
        &self.inner
    }

    pub fn cipher(&self) -> &Cipher {
        &self.cipher
    }

    fn enc_name(&self, name: &str) -> Result<String, RemoteError> {
        if name.len() > MAX_NAME_BYTES {
            return Err(RemoteError::InvalidName(format!(
                "\"{name}\" is too long ({} bytes; encrypted names can be at most {MAX_NAME_BYTES})",
                name.len()
            )));
        }
        self.cipher.encrypt_name(name).map_err(|e| RemoteError::InvalidName(e.to_string()))
    }

    /// Converts an inner entry to plaintext, or `None` if its name doesn't decrypt.
    fn decrypt_entry(&self, mut e: RemoteEntry) -> Option<RemoteEntry> {
        e.name = self.cipher.decrypt_name(&e.name).ok()?;
        if e.kind == Kind::File {
            e.size = crypt::decrypted_size(e.size).ok()?;
        }
        Some(e)
    }

    fn map_entry(&self, mut e: RemoteEntry, plain_name: &str, plain_size: Option<u64>) -> RemoteEntry {
        e.name = plain_name.to_string();
        if e.kind == Kind::File {
            e.size = plain_size.unwrap_or_else(|| crypt::decrypted_size(e.size).unwrap_or(0));
        }
        e
    }

    fn inner_entry(&self, e: &RemoteEntry) -> Result<RemoteEntry, RemoteError> {
        let mut raw = e.clone();
        raw.name = self.enc_name(&e.name)?;
        if raw.kind == Kind::File {
            raw.size = crypt::encrypted_size(e.size);
        }
        Ok(raw)
    }

    async fn file_nonce(&self, raw: &RemoteEntry) -> Result<Nonce, RemoteError> {
        if let Some(n) = self.nonces.lock().unwrap().get(&raw.id) {
            return Ok(*n);
        }
        let header = super::read_all(self.inner.read(raw, 0, Some(HEADER_SIZE as u64)).await?)
            .await
            .map_err(|e| RemoteError::Network(e.to_string()))?;
        let nonce = Decryptor::parse_header(&header).map_err(|e| RemoteError::Integrity(e.to_string()))?;
        let mut cache = self.nonces.lock().unwrap();
        if cache.len() > 4096 {
            cache.clear();
        }
        cache.insert(raw.id.clone(), nonce);
        Ok(nonce)
    }
}

fn crypt_err(e: CryptError) -> std::io::Error {
    std::io::Error::new(std::io::ErrorKind::InvalidData, e)
}

/// Turns a plaintext stream into an rclone-crypt stream (header + sealed 64 KiB blocks).
pub fn encrypt_stream(cipher: Cipher, input: ByteStream) -> ByteStream {
    struct State {
        input: ByteStream,
        enc: Encryptor,
        buf: BytesMut,
        header_sent: bool,
        done: bool,
    }
    let state = State { input, enc: Encryptor::new(cipher, Nonce::random()), buf: BytesMut::new(), header_sent: false, done: false };
    Box::pin(futures::stream::unfold(state, |mut st| async move {
        if !st.header_sent {
            st.header_sent = true;
            let h = Bytes::copy_from_slice(st.enc.header());
            return Some((Ok(h), st));
        }
        loop {
            if st.buf.len() >= BLOCK_DATA_SIZE {
                let block = st.buf.split_to(BLOCK_DATA_SIZE);
                let mut out = Vec::with_capacity(BLOCK_DATA_SIZE + 16);
                st.enc.encrypt_block(&block, &mut out);
                return Some((Ok(Bytes::from(out)), st));
            }
            if st.done {
                if st.buf.is_empty() {
                    return None;
                }
                let block = st.buf.split();
                let mut out = Vec::with_capacity(block.len() + 16);
                st.enc.encrypt_block(&block, &mut out);
                return Some((Ok(Bytes::from(out)), st));
            }
            match st.input.next().await {
                Some(Ok(chunk)) => st.buf.extend_from_slice(&chunk),
                Some(Err(e)) => {
                    st.done = true;
                    st.buf.clear();
                    return Some((Err(e), st));
                }
                None => st.done = true,
            }
        }
    }))
}

/// Decrypts an encrypted stream that starts either at the header (`nonce == None`) or at the
/// start of block `first_block`, dropping `discard` leading bytes and stopping after `limit`.
fn decrypt_stream(cipher: Cipher, input: ByteStream, start: Option<(Nonce, u64)>, discard: u64, limit: Option<u64>) -> ByteStream {
    struct State {
        input: ByteStream,
        dec: Decryptor,
        discard: u64,
        remaining: Option<u64>,
        finished: bool,
    }
    let dec = match start {
        Some((nonce, block)) => Decryptor::at_block(cipher, nonce, block),
        None => Decryptor::from_start(cipher),
    };
    let state = State { input, dec, discard, remaining: limit, finished: false };
    Box::pin(futures::stream::unfold(state, |mut st| async move {
        loop {
            if st.finished || st.remaining == Some(0) {
                return None;
            }
            let mut out = Vec::new();
            match st.input.next().await {
                Some(Ok(chunk)) => {
                    if let Err(e) = st.dec.feed(&chunk, &mut out) {
                        st.finished = true;
                        return Some((Err(crypt_err(e)), st));
                    }
                }
                Some(Err(e)) => {
                    st.finished = true;
                    return Some((Err(e), st));
                }
                None => {
                    st.finished = true;
                    if let Err(e) = st.dec.finish(&mut out) {
                        return Some((Err(crypt_err(e)), st));
                    }
                }
            }
            let mut data = Bytes::from(out);
            if st.discard > 0 {
                let d = st.discard.min(data.len() as u64);
                data = data.slice(d as usize..);
                st.discard -= d;
            }
            if let Some(rem) = st.remaining.as_mut() {
                if data.len() as u64 > *rem {
                    data = data.slice(..*rem as usize);
                }
                *rem -= data.len() as u64;
            }
            if !data.is_empty() {
                return Some((Ok(data), st));
            }
        }
    }))
}

#[async_trait]
impl<R: Remote> Remote for CryptRemote<R> {
    async fn scan(&self) -> Result<RemoteTree, RemoteError> {
        let raw = self.inner.scan().await?;
        let mut tree = RemoteTree { root_id: raw.root_id, entries: Vec::with_capacity(raw.entries.len()), skipped: raw.skipped };
        for e in raw.entries {
            let name = e.name.clone();
            match self.decrypt_entry(e) {
                Some(p) => tree.entries.push(p),
                None => tree.skipped.push(name),
            }
        }
        Ok(tree)
    }

    async fn create_folder(&self, parent_id: &str, name: &str) -> Result<RemoteEntry, RemoteError> {
        let enc = self.enc_name(name)?;
        let e = self.inner.create_folder(parent_id, &enc).await?;
        Ok(self.map_entry(e, name, None))
    }

    async fn upload(&self, parent_id: &str, name: &str, body: UploadBody) -> Result<RemoteEntry, RemoteError> {
        let enc_name = self.enc_name(name)?;
        let plain_size = body.size;
        let stream = encrypt_stream(self.cipher.clone(), body.stream);
        let e = self
            .inner
            .upload(parent_id, &enc_name, UploadBody { stream, size: crypt::encrypted_size(plain_size), mtime_ms: body.mtime_ms })
            .await?;
        Ok(self.map_entry(e, name, Some(plain_size)))
    }

    async fn read(&self, file: &RemoteEntry, offset: u64, len: Option<u64>) -> Result<ByteStream, RemoteError> {
        let raw = self.inner_entry(file)?;
        if len == Some(0) || offset >= file.size {
            return Ok(Box::pin(futures::stream::empty()));
        }
        let len = len.map(|l| l.min(file.size - offset));
        if offset == 0 {
            let inner_len = len.map(|l| {
                let r = crypt::underlying_range(0, l);
                r.offset + r.len
            });
            let s = self.inner.read(&raw, 0, inner_len).await?;
            return Ok(decrypt_stream(self.cipher.clone(), s, None, 0, len));
        }
        let nonce = self.file_nonce(&raw).await?;
        let r = crypt::underlying_range(offset, len.unwrap_or(file.size - offset));
        let s = self.inner.read(&raw, r.offset, len.map(|_| r.len)).await?;
        Ok(decrypt_stream(self.cipher.clone(), s, Some((nonce, r.first_block)), r.discard, len))
    }

    async fn rename(&self, entry: &RemoteEntry, new_parent_id: &str, new_name: &str) -> Result<RemoteEntry, RemoteError> {
        let raw = self.inner_entry(entry)?;
        let enc = self.enc_name(new_name)?;
        let e = self.inner.rename(&raw, new_parent_id, &enc).await?;
        Ok(self.map_entry(e, new_name, Some(entry.size)))
    }

    async fn trash(&self, entry: &RemoteEntry) -> Result<(), RemoteError> {
        let raw = self.inner_entry(entry)?;
        self.inner.trash(&raw).await
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::remote::mem::{MemRemote, ROOT};
    use crate::remote::read_all;

    fn setup() -> (Arc<MemRemote>, CryptRemote<MemRemote>) {
        let mem = Arc::new(MemRemote::new());
        let c = CryptRemote::new(mem.clone(), Cipher::new("pw", "salt").unwrap());
        (mem, c)
    }

    #[tokio::test]
    async fn upload_scan_read_ranges() {
        let (mem, c) = setup();
        let dir = c.create_folder(ROOT, "Photos").await.unwrap();
        let data: Vec<u8> = (0..300_000u32).map(|i| (i % 241) as u8).collect();
        let f = c.upload(&dir.id, "big.raw", UploadBody::from_bytes(data.clone(), 1234)).await.unwrap();
        assert_eq!(f.size, data.len() as u64);
        assert_eq!(f.name, "big.raw");

        // Inner names are encrypted.
        assert!(mem.snapshot().keys().all(|k| !k.contains("Photos") && !k.contains("big.raw")));

        let tree = c.scan().await.unwrap();
        let names: Vec<_> = tree.entries.iter().map(|e| e.name.as_str()).collect();
        assert!(names.contains(&"Photos") && names.contains(&"big.raw"));
        let f = tree.entries.iter().find(|e| e.name == "big.raw").unwrap().clone();
        assert_eq!(f.size, data.len() as u64);

        assert_eq!(read_all(c.read(&f, 0, None).await.unwrap()).await.unwrap(), data);
        for (off, len) in [(0u64, 10u64), (1, 65_536), (65_535, 2), (65_536, 65_536), (200_000, 100_000), (299_999, 50)] {
            let got = read_all(c.read(&f, off, Some(len)).await.unwrap()).await.unwrap();
            let end = ((off + len) as usize).min(data.len());
            assert_eq!(got, &data[off as usize..end], "range {off}+{len}");
        }
        let tail = read_all(c.read(&f, 123_456, None).await.unwrap()).await.unwrap();
        assert_eq!(tail, &data[123_456..]);
    }

    #[tokio::test]
    async fn foreign_files_are_skipped_and_long_names_rejected() {
        let (mem, c) = setup();
        mem.put_file(ROOT, "readme.txt", b"plain", 0);
        let tree = c.scan().await.unwrap();
        assert!(tree.entries.is_empty());
        assert_eq!(tree.skipped, vec!["readme.txt".to_string()]);
        let long = "a".repeat(MAX_NAME_BYTES + 1);
        assert!(matches!(c.create_folder(ROOT, &long).await, Err(RemoteError::InvalidName(_))));
        let ok = "b".repeat(MAX_NAME_BYTES);
        let e = c.create_folder(ROOT, &ok).await.unwrap();
        let raw_len = mem.snapshot().keys().map(|k| k.trim_end_matches('/').len()).max().unwrap();
        assert!(raw_len <= 255, "{raw_len}");
        assert_eq!(e.name, ok);
    }

    #[tokio::test]
    async fn rename_and_empty_file() {
        let (_mem, c) = setup();
        let f = c.upload(ROOT, "empty", UploadBody::from_bytes(Vec::new(), 0)).await.unwrap();
        assert_eq!(read_all(c.read(&f, 0, None).await.unwrap()).await.unwrap(), Vec::<u8>::new());
        let r = c.rename(&f, ROOT, "renamed").await.unwrap();
        assert_eq!(r.name, "renamed");
        let tree = c.scan().await.unwrap();
        assert_eq!(tree.entries[0].name, "renamed");
    }
}
