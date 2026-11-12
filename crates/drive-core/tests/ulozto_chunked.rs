//! Resumable uploads against a fake uloz.to with network faults: interruptions, restarts,
//! server errors, throttling, stalls, corruption, expired batches and fallbacks.

mod support;

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use bytes::Bytes;
use drive_core::crypt::{self, Cipher};
use drive_core::remote::crypt::CryptRemote;
use drive_core::remote::resumable::{
    BytesSource, NoMeter, Progress, ResumableUpload, ResumeState, UploadSource,
};
use drive_core::remote::{Remote, RemoteEntry, RemoteError, read_all};
use drive_core::ulozto::{UlozClient, UlozConfig, UlozRemote};
use support::fake_ulozto::{Fake, ROOT};
use tokio_util::sync::CancellationToken;

const CHUNK: u64 = 64 * 1024;

fn client(fake: &Fake, tweak: impl FnOnce(&mut UlozConfig)) -> Arc<UlozClient> {
    let mut cfg = UlozConfig::new("me", "pw", "test-device");
    cfg.api_root = fake.url.clone();
    cfg.chunked_min_size = 2 * CHUNK;
    cfg.chunk_size = Some(CHUNK);
    cfg.stall_timeout = Duration::from_secs(2);
    cfg.response_wait = Duration::from_secs(10);
    cfg.status_poll = Duration::from_millis(20);
    tweak(&mut cfg);
    Arc::new(UlozClient::new(cfg).unwrap())
}

fn crypt_remote(c: Arc<UlozClient>) -> CryptRemote<UlozRemote> {
    CryptRemote::new(
        Arc::new(UlozRemote::new(c, ROOT.into())),
        Cipher::new("pw", "salt").unwrap(),
    )
}

fn data(len: usize, seed: u8) -> Vec<u8> {
    (0..len)
        .map(|i| ((i * 31 + seed as usize * 7) % 251) as u8)
        .collect()
}

struct Run {
    saved: Arc<Mutex<ResumeState>>,
    progress: Arc<Progress>,
    cancel: CancellationToken,
}

impl Run {
    fn new(initial: ResumeState) -> Self {
        Self {
            saved: Arc::new(Mutex::new(initial)),
            progress: Arc::new(Progress::default()),
            cancel: CancellationToken::new(),
        }
    }

    fn upload(&self, source: Arc<dyn UploadSource>) -> ResumableUpload {
        let saved = self.saved.clone();
        ResumableUpload {
            source,
            mtime_ms: 1_700_000_000_000,
            state: self.saved.lock().unwrap().clone(),
            checkpoint: Arc::new(move |s: &ResumeState| *saved.lock().unwrap() = s.clone()),
            meter: Arc::new(NoMeter),
            progress: self.progress.clone(),
            scale: 1.0,
            cancel: self.cancel.clone(),
        }
    }

    fn state(&self) -> ResumeState {
        self.saved.lock().unwrap().clone()
    }
}

fn plain_of(fake: &Fake) -> Vec<u8> {
    let files = fake.committed_files();
    assert_eq!(files.len(), 1, "exactly one committed file");
    crypt::decrypt_bytes(&Cipher::new("pw", "salt").unwrap(), &files[0].data).unwrap()
}

async fn up(
    remote: &CryptRemote<UlozRemote>,
    run: &Run,
    plain: &[u8],
) -> Result<RemoteEntry, RemoteError> {
    let src: Arc<dyn UploadSource> = Arc::new(BytesSource(Bytes::from(plain.to_vec())));
    remote
        .upload_resumable(ROOT, "video.mp4", run.upload(src))
        .await
}

#[tokio::test]
async fn chunked_upload_roundtrips_and_is_verified() {
    let fake = Fake::start().await;
    let remote = crypt_remote(client(&fake, |_| {}));
    let plain = data(1_000_000, 1);
    let run = Run::new(ResumeState::default());
    let entry = up(&remote, &run, &plain).await.unwrap();
    assert_eq!(entry.size, plain.len() as u64);
    assert_eq!(entry.name, "video.mp4");
    let s = fake.stats();
    let chunks = crypt::encrypted_size(plain.len() as u64).div_ceil(CHUNK) as u32;
    assert_eq!(s.chunk_posts, chunks, "each chunk sent once");
    assert_eq!((s.registers, s.hash_verifies, s.single_uploads), (1, 1, 0));
    assert_eq!(s.missing_content_length, 0, "chunks carry Content-Length");
    assert_eq!(plain_of(&fake), plain);
    // Progress ends at the plaintext size.
    assert_eq!(run.progress.done(), plain.len() as u64);
    // Read it back through the download path, whole and ranged.
    assert_eq!(
        read_all(remote.read(&entry, 0, None).await.unwrap())
            .await
            .unwrap(),
        plain
    );
    let part = read_all(remote.read(&entry, 300_000, Some(70_000)).await.unwrap())
        .await
        .unwrap();
    assert_eq!(part, &plain[300_000..370_000]);
    // The rclone metadata (mtime) made it into the description.
    let f = &fake.committed_files()[0];
    let meta = drive_core::ulozto::meta::decode(&f.description).unwrap();
    assert_eq!(meta.mtime_micros, 1_700_000_000_000 * 1000);
}

#[tokio::test]
async fn a_chunk_stuck_on_a_slow_connection_is_sent_again_on_a_new_one() {
    let fake = Fake::start().await;
    let plain = data(1_000_000, 4);
    let chunks = crypt::encrypted_size(plain.len() as u64).div_ceil(CHUNK) as u32;
    let slow = chunks - 2; // late enough that earlier chunks tell what's usual
    fake.faults(|f| f.slow_chunk = Some((slow, Duration::from_secs(60))));
    let remote = crypt_remote(client(&fake, |c| c.max_chunk_streams = 2));
    let run = Run::new(ResumeState::default());
    let started = Instant::now();
    up(&remote, &run, &plain).await.unwrap();
    // Waited a few seconds on the slow connection, not the whole minute.
    assert!(
        started.elapsed() < Duration::from_secs(20),
        "{:?}",
        started.elapsed()
    );
    let s = fake.stats();
    assert_eq!(s.chunk_posts_by_number[&slow], 2, "sent again once");
    assert_eq!(s.chunk_posts, chunks + 1);
    assert_eq!(plain_of(&fake), plain);
    assert_eq!(run.progress.done(), plain.len() as u64);
}

#[tokio::test]
async fn uniformly_slow_chunks_are_not_sent_again() {
    // A slow link (every chunk slow) is not a bad connection.
    let fake = Fake::start().await;
    fake.faults(|f| f.chunk_delay = Duration::from_millis(400));
    let plain = data(700_000, 5);
    let chunks = crypt::encrypted_size(plain.len() as u64).div_ceil(CHUNK) as u32;
    let remote = crypt_remote(client(&fake, |_| {}));
    let run = Run::new(ResumeState::default());
    up(&remote, &run, &plain).await.unwrap();
    assert_eq!(fake.stats().chunk_posts, chunks, "each chunk sent once");
}

#[tokio::test]
async fn resumes_after_restart_without_resending_confirmed_chunks() {
    let fake = Fake::start().await;
    fake.faults(|f| f.chunk_delay = Duration::from_millis(30));
    let plain = data(1_500_000, 2);
    let total_chunks = crypt::encrypted_size(plain.len() as u64).div_ceil(CHUNK) as u32;

    // First attempt: cancel ("pause" / app quits) after a few chunks are confirmed.
    let run = Run::new(ResumeState::default());
    let remote = crypt_remote(client(&fake, |c| c.max_chunk_streams = 2));
    let progress = run.progress.clone();
    let cancel = run.cancel.clone();
    tokio::spawn(async move {
        while progress.confirmed.load(Ordering::Relaxed) < 6 * CHUNK {
            tokio::time::sleep(Duration::from_millis(5)).await;
        }
        cancel.cancel();
    });
    let r = up(&remote, &run, &plain).await;
    assert!(matches!(r, Err(RemoteError::Cancelled)), "{r:?}");
    let saved = run.state();
    assert!(saved.0.contains_key("crypt") && saved.0.contains_key("ulozto"));
    let sent_before = fake.stats().chunk_posts;
    assert!(sent_before < total_chunks);

    // "Restart": a brand-new client and remote, only the saved state survives.
    let run2 = Run::new(saved);
    let remote2 = crypt_remote(client(&fake, |_| {}));
    let entry = up(&remote2, &run2, &plain).await.unwrap();
    assert_eq!(entry.size, plain.len() as u64);
    let s = fake.stats();
    assert_eq!(s.new_batches, 1, "the same batch was continued");
    // Every chunk sent once, plus at most the ones in flight at the moment of cancelling.
    assert!(
        s.chunk_posts <= total_chunks + 2,
        "{} posts for {total_chunks} chunks",
        s.chunk_posts
    );
    assert!(s.chunk_posts_by_number.values().all(|&n| n <= 2));
    assert_eq!(
        plain_of(&fake),
        plain,
        "resumed file decrypts to the original (same nonce)"
    );
}

#[tokio::test]
async fn transient_errors_and_throttling_are_retried() {
    let fake = Fake::start().await;
    fake.faults(|f| {
        f.fail_chunks = 3;
        f.throttle_chunks = 2;
        f.processing_polls = 3;
    });
    let c = client(&fake, |c| c.max_chunk_streams = 4);
    let remote = crypt_remote(c.clone());
    let plain = data(800_000, 3);
    let run = Run::new(ResumeState::default());
    up(&remote, &run, &plain).await.unwrap();
    assert_eq!(plain_of(&fake), plain);
    let s = fake.stats();
    assert!(s.max_concurrent <= 4);
}

#[tokio::test]
async fn parallel_chunks_make_one_file_faster() {
    let fake = Fake::start().await;
    fake.faults(|f| f.chunk_delay = Duration::from_millis(40));
    let remote = crypt_remote(client(&fake, |c| c.max_chunk_streams = 4));
    let plain = data(2_500_000, 4);
    let run = Run::new(ResumeState::default());
    up(&remote, &run, &plain).await.unwrap();
    assert!(
        fake.stats().max_concurrent >= 2,
        "chunks of one file go out in parallel"
    );
}

#[tokio::test]
async fn stalled_chunk_is_abandoned_and_resent() {
    let fake = Fake::start().await;
    fake.faults(|f| f.stall_chunk = Some(3));
    let remote = crypt_remote(client(&fake, |c| {
        c.stall_timeout = Duration::from_millis(800)
    }));
    let plain = data(600_000, 5);
    let run = Run::new(ResumeState::default());
    let started = Instant::now();
    up(&remote, &run, &plain).await.unwrap();
    assert!(
        started.elapsed() < Duration::from_secs(15),
        "watchdog gave up on the stalled chunk"
    );
    assert_eq!(fake.stats().chunk_posts_by_number[&3], 2);
    assert_eq!(plain_of(&fake), plain);
}

#[tokio::test]
async fn corrupted_assembly_is_rejected_and_discarded() {
    let fake = Fake::start().await;
    fake.faults(|f| f.corrupt_assembly = true);
    let remote = crypt_remote(client(&fake, |_| {}));
    let plain = data(400_000, 6);
    let run = Run::new(ResumeState::default());
    let r = up(&remote, &run, &plain).await;
    assert!(matches!(r, Err(RemoteError::Integrity(_))), "{r:?}");
    assert!(fake.committed_files().is_empty(), "nothing committed");
    assert_eq!(fake.stats().discards, 1);
    // A fresh attempt (the engine drops the state after an integrity error) succeeds.
    fake.faults(|f| f.corrupt_assembly = false);
    up(&remote, &Run::new(ResumeState::default()), &plain)
        .await
        .unwrap();
    assert_eq!(plain_of(&fake), plain);
}

/// A file whose content changes after its first chunk has been read.
struct ChangingSource {
    data: Vec<u8>,
    reads: std::sync::atomic::AtomicU32,
    changed: AtomicBool,
}

impl UploadSource for ChangingSource {
    fn len(&self) -> u64 {
        self.data.len() as u64
    }
    fn read_at(&self, offset: u64, buf: &mut [u8]) -> std::io::Result<()> {
        buf.copy_from_slice(&self.data[offset as usize..offset as usize + buf.len()]);
        // After the hashing pass (one full read), flip a byte in everything read later.
        let n = self.reads.fetch_add(1, Ordering::Relaxed);
        if n > (self.data.len() as u64).div_ceil(CHUNK) as u32 + 2 {
            self.changed.store(true, Ordering::Relaxed);
            buf[0] ^= 0xff;
        }
        Ok(())
    }
}

#[tokio::test]
async fn file_changing_during_upload_aborts_before_sending_it() {
    let fake = Fake::start().await;
    // Raw remote (no crypt) so reads map 1:1 to chunks.
    let remote = UlozRemote::new(client(&fake, |c| c.max_chunk_streams = 1), ROOT.into());
    let src = Arc::new(ChangingSource {
        data: data(700_000, 7),
        reads: 0.into(),
        changed: AtomicBool::new(false),
    });
    let run = Run::new(ResumeState::default());
    let r = remote
        .upload_resumable(ROOT, "doc.bin", run.upload(src.clone()))
        .await;
    assert!(
        matches!(&r, Err(RemoteError::Integrity(m)) if m.contains("changed")),
        "{r:?}"
    );
    assert!(fake.committed_files().is_empty());
}

#[tokio::test]
async fn falls_back_to_single_request_when_chunked_is_unavailable() {
    let fake = Fake::start().await;
    fake.faults(|f| f.chunked_disabled = true);
    let remote = crypt_remote(client(&fake, |_| {}));
    let plain = data(500_000, 8);
    up(&remote, &Run::new(ResumeState::default()), &plain)
        .await
        .unwrap();
    let s = fake.stats();
    assert_eq!((s.single_uploads, s.chunk_posts), (1, 0));
    assert_eq!(plain_of(&fake), plain);
    // And the remote stops advertising resumable uploads for a while.
    assert_eq!(remote.resumable_min_size(), None);
}

#[tokio::test]
async fn small_files_use_a_single_request() {
    let fake = Fake::start().await;
    let remote = crypt_remote(client(&fake, |_| {}));
    let plain = data(50_000, 9);
    let run = Run::new(ResumeState::default());
    up(&remote, &run, &plain).await.unwrap();
    assert_eq!(
        (fake.stats().single_uploads, fake.stats().chunk_posts),
        (1, 0)
    );
    assert!(
        !run.state().0.contains_key("crypt"),
        "no nonce saved for non-resumable uploads"
    );
    assert_eq!(plain_of(&fake), plain);
}

#[tokio::test]
async fn expired_batch_starts_over_with_a_new_one() {
    let fake = Fake::start().await;
    fake.faults(|f| f.chunk_delay = Duration::from_millis(20));
    let plain = data(900_000, 10);
    let run = Run::new(ResumeState::default());
    let remote = crypt_remote(client(&fake, |c| c.max_chunk_streams = 1));
    let (progress, cancel) = (run.progress.clone(), run.cancel.clone());
    tokio::spawn(async move {
        while progress.confirmed.load(Ordering::Relaxed) < 3 * CHUNK {
            tokio::time::sleep(Duration::from_millis(5)).await;
        }
        cancel.cancel();
    });
    assert!(matches!(
        up(&remote, &run, &plain).await,
        Err(RemoteError::Cancelled)
    ));
    fake.faults(|f| {
        f.expire_batches = true;
        f.chunk_delay = Duration::ZERO;
    });
    let run2 = Run::new(run.state());
    up(&crypt_remote(client(&fake, |_| {})), &run2, &plain)
        .await
        .unwrap();
    assert_eq!(fake.stats().new_batches, 2);
    assert_eq!(plain_of(&fake), plain);
}

#[tokio::test]
async fn lost_commit_reply_adopts_the_committed_file() {
    let fake = Fake::start().await;
    fake.faults(|f| f.lose_commit_reply = true);
    let remote = crypt_remote(client(&fake, |_| {}));
    let plain = data(400_000, 11);
    let entry = up(&remote, &Run::new(ResumeState::default()), &plain)
        .await
        .unwrap();
    assert_eq!(entry.size, plain.len() as u64);
    assert_eq!(fake.committed_files().len(), 1, "no duplicate upload");
    assert_eq!(fake.stats().new_batches, 1);
}

#[tokio::test]
async fn crash_after_commit_resumes_by_adopting_not_reuploading() {
    let fake = Fake::start().await;
    let remote = crypt_remote(client(&fake, |_| {}));
    let plain = data(500_000, 12);
    let run = Run::new(ResumeState::default());
    up(&remote, &run, &plain).await.unwrap();
    // The engine would delete the state on success; pretend it crashed before doing so.
    let leftover = run.state();
    assert!(drive_core::ulozto::chunked::finished_slug(&leftover).is_some());
    let posts = fake.stats().chunk_posts;
    let again = up(
        &crypt_remote(client(&fake, |_| {})),
        &Run::new(leftover),
        &plain,
    )
    .await
    .unwrap();
    assert_eq!(again.size, plain.len() as u64);
    assert_eq!(fake.stats().chunk_posts, posts, "nothing re-sent");
    assert_eq!(fake.committed_files().len(), 1, "no duplicate");
}
