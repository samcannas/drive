//! Bandwidth limiting and transfer progress for byte streams.

use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::Duration;

use tokio::time::Instant;

use futures::StreamExt;

use crate::remote::ByteStream;
use crate::remote::resumable::{Meter, Progress};

/// Token bucket shared by all transfers in one direction.
///
/// Two limits apply, the lower one winning: the user's (`set_kbps`, 0 = none) and an
/// automatic one with an optional pause (`set_auto`, see [`super::adaptive`]). The automatic
/// limit and pause expire on their own unless renewed, so a stopped controller can never
/// leave uploads slowed down or paused.
pub struct Throttle {
    rate_bytes: AtomicU64,
    /// Automatic limit in bytes/s (0 = none), valid until `auto_until` (ms since `epoch`).
    auto_bytes: AtomicU64,
    auto_until: AtomicU64,
    /// Paused until this time (ms since `epoch`).
    hold_until: AtomicU64,
    /// Bytes let through so far.
    passed: AtomicU64,
    epoch: Instant,
    state: tokio::sync::Mutex<(f64, Instant)>,
}

impl Throttle {
    pub fn new(kbps: u64) -> Self {
        Self {
            rate_bytes: AtomicU64::new(kbps * 1024),
            auto_bytes: AtomicU64::new(0),
            auto_until: AtomicU64::new(0),
            hold_until: AtomicU64::new(0),
            passed: AtomicU64::new(0),
            epoch: Instant::now(),
            state: tokio::sync::Mutex::new((0.0, Instant::now())),
        }
    }

    pub fn set_kbps(&self, kbps: u64) {
        self.rate_bytes.store(kbps * 1024, Ordering::Relaxed);
    }

    fn now_ms(&self) -> u64 {
        self.epoch.elapsed().as_millis() as u64
    }

    /// Sets the automatic limit (bytes/s) and pause, both lapsing after `valid_for`.
    pub fn set_auto(&self, limit: Option<u64>, hold: bool, valid_for: Duration) {
        let until = self.now_ms() + valid_for.as_millis() as u64;
        self.auto_bytes.store(limit.unwrap_or(0), Ordering::Relaxed);
        self.auto_until
            .store(if limit.is_some() { until } else { 0 }, Ordering::Relaxed);
        self.hold_until
            .store(if hold { until } else { 0 }, Ordering::Relaxed);
    }

    /// Bytes let through so far.
    pub fn passed(&self) -> u64 {
        self.passed.load(Ordering::Relaxed)
    }

    /// The limit in force now, bytes/s (0 = none).
    pub fn effective_rate(&self) -> u64 {
        let user = self.rate_bytes.load(Ordering::Relaxed);
        let auto = if self.now_ms() < self.auto_until.load(Ordering::Relaxed) {
            self.auto_bytes.load(Ordering::Relaxed)
        } else {
            0
        };
        match (user, auto) {
            (0, a) => a,
            (u, 0) => u,
            (u, a) => u.min(a),
        }
    }

    pub async fn consume(&self, bytes: usize) {
        // Paused: wait until the pause ends or lapses.
        loop {
            let until = self.hold_until.load(Ordering::Relaxed);
            let now = self.now_ms();
            if now >= until {
                break;
            }
            tokio::time::sleep(Duration::from_millis((until - now).min(50))).await;
        }
        self.passed.fetch_add(bytes as u64, Ordering::Relaxed);
        let rate = self.effective_rate() as f64;
        if rate <= 0.0 {
            return;
        }
        let wait = {
            let mut st = self.state.lock().await;
            let now = Instant::now();
            let elapsed = now.duration_since(st.1).as_secs_f64();
            // Allow at most one second of burst.
            st.0 = (st.0 + elapsed * rate).min(rate);
            st.1 = now;
            st.0 -= bytes as f64;
            if st.0 < 0.0 {
                Duration::from_secs_f64(-st.0 / rate)
            } else {
                Duration::ZERO
            }
        };
        if !wait.is_zero() {
            tokio::time::sleep(wait).await;
        }
    }
}

/// Wraps a stream so every chunk is counted as progress and passes through `throttle`.
pub fn metered(stream: ByteStream, progress: Arc<Progress>, throttle: Arc<Throttle>) -> ByteStream {
    Box::pin(stream.then(move |chunk| {
        let progress = progress.clone();
        let throttle = throttle.clone();
        async move {
            if let Ok(b) = &chunk {
                throttle.consume(b.len()).await;
                progress
                    .confirmed
                    .fetch_add(b.len() as u64, Ordering::Relaxed);
            }
            chunk
        }
    }))
}

/// Paces uploads through a [`Throttle`].
pub struct ThrottleMeter(pub Arc<Throttle>);

#[async_trait::async_trait]
impl Meter for ThrottleMeter {
    async fn consume(&self, bytes: usize) {
        self.0.consume(bytes).await;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test(start_paused = true)]
    async fn limits_rate() {
        let t = Throttle::new(100); // 100 KiB/s
        let start = tokio::time::Instant::now();
        for _ in 0..4 {
            t.consume(100 * 1024).await;
        }
        // The first second is free credit; the rest must wait about three seconds.
        let took = start.elapsed();
        assert!(took >= Duration::from_millis(2900), "{took:?}");
    }

    #[tokio::test(start_paused = true)]
    async fn unlimited_by_default_and_counts_bytes() {
        let t = Throttle::new(0);
        let start = tokio::time::Instant::now();
        for _ in 0..100 {
            t.consume(1024 * 1024).await;
        }
        assert_eq!(start.elapsed(), Duration::ZERO);
        assert_eq!(t.passed(), 100 * 1024 * 1024);
    }

    #[tokio::test(start_paused = true)]
    async fn the_lower_of_user_and_automatic_limit_applies() {
        let t = Throttle::new(1000);
        assert_eq!(t.effective_rate(), 1000 * 1024);
        t.set_auto(Some(100 * 1024), false, Duration::from_secs(10));
        assert_eq!(t.effective_rate(), 100 * 1024);
        t.set_auto(Some(10_000 * 1024), false, Duration::from_secs(10));
        assert_eq!(t.effective_rate(), 1000 * 1024);
        t.set_kbps(0);
        assert_eq!(t.effective_rate(), 10_000 * 1024);
        t.set_auto(None, false, Duration::from_secs(10));
        assert_eq!(t.effective_rate(), 0);
    }

    #[tokio::test(start_paused = true)]
    async fn the_automatic_limit_lapses_unless_renewed() {
        let t = Throttle::new(0);
        t.set_auto(Some(1024), false, Duration::from_secs(10));
        assert_eq!(t.effective_rate(), 1024);
        tokio::time::advance(Duration::from_secs(11)).await;
        assert_eq!(
            t.effective_rate(),
            0,
            "a stopped controller must not leave a limit"
        );
    }

    #[tokio::test(start_paused = true)]
    async fn a_pause_holds_uploads_until_it_lapses() {
        let t = Throttle::new(0);
        t.set_auto(None, true, Duration::from_secs(2));
        let start = tokio::time::Instant::now();
        t.consume(1).await;
        let waited = start.elapsed();
        assert!(
            waited >= Duration::from_secs(2) && waited < Duration::from_millis(2100),
            "{waited:?}"
        );
    }

    #[tokio::test(start_paused = true)]
    async fn ending_a_pause_releases_waiting_uploads() {
        let t = Arc::new(Throttle::new(0));
        t.set_auto(None, true, Duration::from_secs(60));
        let t2 = t.clone();
        let start = tokio::time::Instant::now();
        let h = tokio::spawn(async move { t2.consume(1).await });
        tokio::time::sleep(Duration::from_millis(500)).await;
        t.set_auto(None, false, Duration::ZERO);
        h.await.unwrap();
        assert!(
            start.elapsed() < Duration::from_millis(600),
            "{:?}",
            start.elapsed()
        );
    }
}
