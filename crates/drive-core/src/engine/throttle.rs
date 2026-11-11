//! Bandwidth limiting and transfer progress for byte streams.

use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::Duration;

use tokio::time::Instant;

use futures::StreamExt;

use crate::remote::ByteStream;
use crate::remote::resumable::{Meter, Progress};

/// Token bucket shared by all transfers in one direction. A rate of 0 means unlimited.
pub struct Throttle {
    rate_bytes: AtomicU64,
    state: tokio::sync::Mutex<(f64, Instant)>,
}

impl Throttle {
    pub fn new(kbps: u64) -> Self {
        Self {
            rate_bytes: AtomicU64::new(kbps * 1024),
            state: tokio::sync::Mutex::new((0.0, Instant::now())),
        }
    }

    pub fn set_kbps(&self, kbps: u64) {
        self.rate_bytes.store(kbps * 1024, Ordering::Relaxed);
    }

    pub async fn consume(&self, bytes: usize) {
        let rate = self.rate_bytes.load(Ordering::Relaxed) as f64;
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
}
