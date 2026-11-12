//! Automatic upload speed on a real (shaped) network: real TCP, real kernel queues and
//! socket buffers, the real controller, probe and throttle.
//!
//! Each test needs loopback shaped by root first (CI does this on Linux, see ci.yml):
//!
//! ```sh
//! sudo ip link set lo mtu 1500
//! sudo ethtool -K lo tso off gso off gro off
//! # bloated: 8 Mbit/s with a one-second buffer
//! sudo tc qdisc add dev lo root tbf rate 8mbit burst 96kb limit 1000000
//! DRIVE_NETEM=bloated cargo test -p drive-core --test adaptive_netem -- --ignored --nocapture
//! # healthy: the same speed with a short (~30 ms) buffer
//! sudo tc qdisc change dev lo root tbf rate 8mbit burst 96kb limit 30000
//! DRIVE_NETEM=healthy cargo test -p drive-core --test adaptive_netem -- --ignored --nocapture
//! sudo tc qdisc del dev lo root
//! ```
//!
//! Another connection's handshake time is measured throughout, independently of the
//! controller's own probe: that is what a video call or a web page would experience.

use std::net::SocketAddr;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use drive_core::engine::adaptive::{self, Mode, Params, Snapshot, TcpProbe};
use drive_core::engine::throttle::Throttle;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::{TcpListener, TcpStream};
use tokio::time::Instant;
use tokio_util::sync::CancellationToken;

/// Runs only when `DRIVE_NETEM` names the loopback shaping in place.
fn shaped(kind: &str) -> bool {
    let on = std::env::var("DRIVE_NETEM").is_ok_and(|v| v == kind);
    if !on {
        eprintln!(
            "DRIVE_NETEM is not {kind:?} (needs shaped loopback, see the file comment); skipping"
        );
    }
    on
}

/// Bytes our sockets hold towards `port` (sent but unacknowledged plus not yet sent), from
/// /proc/net/tcp: where data waits that a pause in our uploads cannot stop.
fn socket_backlog(port: u16) -> u64 {
    let Ok(t) = std::fs::read_to_string("/proc/net/tcp") else {
        return 0;
    };
    t.lines()
        .skip(1)
        .filter_map(|l| {
            let f: Vec<&str> = l.split_whitespace().collect();
            let rem_port = u16::from_str_radix(f.get(2)?.rsplit(':').next()?, 16).ok()?;
            let tx = u64::from_str_radix(f.get(4)?.split(':').next()?, 16).ok()?;
            (rem_port == port).then_some(tx)
        })
        .sum()
}

async fn handshake_ms(addr: SocketAddr) -> f64 {
    let start = Instant::now();
    match tokio::time::timeout(Duration::from_secs(5), TcpStream::connect(addr)).await {
        Ok(Ok(_)) => start.elapsed().as_secs_f64() * 1000.0,
        _ => 5000.0,
    }
}

/// A sink that counts the bytes of our uploads (streams tagged 1) and discards the rest.
struct Sink {
    addr: SocketAddr,
    ours: Arc<AtomicU64>,
}

async fn sink() -> Sink {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    let ours = Arc::new(AtomicU64::new(0));
    let counter = ours.clone();
    tokio::spawn(async move {
        loop {
            let Ok((mut s, _)) = listener.accept().await else {
                return;
            };
            let counter = counter.clone();
            tokio::spawn(async move {
                // The first byte says whose stream it is: 1 = ours (counted), 0 = other.
                let mut tag = [0u8; 1];
                let count = s.read_exact(&mut tag).await.is_ok() && tag[0] == 1;
                let mut buf = vec![0u8; 64 * 1024];
                while let Ok(n) = s.read(&mut buf).await {
                    if n == 0 {
                        break;
                    }
                    if count {
                        counter.fetch_add(n as u64, Ordering::Relaxed);
                    }
                }
            });
        }
    });
    Sink { addr, ours }
}

/// A bulk upload: through `throttle` when given (ours), unpaced otherwise (someone else's).
fn bulk_upload(addr: SocketAddr, throttle: Option<Arc<Throttle>>, cancel: CancellationToken) {
    tokio::spawn(async move {
        let mut s = TcpStream::connect(addr).await.unwrap();
        let ours = throttle.is_some();
        if s.write_all(&[ours as u8]).await.is_err() {
            return;
        }
        let block = vec![0x5au8; 64 * 1024];
        while !cancel.is_cancelled() {
            if let Some(t) = &throttle {
                t.consume(block.len()).await;
            }
            if s.write_all(&block).await.is_err() {
                return;
            }
        }
    });
}

struct Rig {
    sink: Sink,
    throttle: Arc<Throttle>,
    enabled: Arc<AtomicBool>,
    shared: Arc<Mutex<Snapshot>>,
    cancel: CancellationToken,
    base: f64,
}

/// Sink, controller (off until `enabled`), and the idle round trip.
async fn rig() -> Rig {
    let _ = tracing_subscriber::fmt()
        .with_env_filter("drive_core=info")
        .with_test_writer()
        .try_init();
    let sink = sink().await;
    let throttle = Arc::new(Throttle::new(0));
    let enabled = Arc::new(AtomicBool::new(false));
    let shared = Arc::new(Mutex::new(Snapshot::default()));
    let cancel = CancellationToken::new();
    {
        let (throttle, enabled, shared, cancel) = (
            throttle.clone(),
            enabled.clone(),
            shared.clone(),
            cancel.clone(),
        );
        let target = sink.addr.to_string();
        tokio::spawn(async move {
            adaptive::run(
                throttle,
                TcpProbe::new(target),
                move || enabled.load(Ordering::Relaxed),
                // Our uploads run for the whole test.
                || true,
                shared,
                Params::default(),
                cancel,
            )
            .await;
        });
    }
    let mut base = f64::MAX;
    for _ in 0..10 {
        base = base.min(handshake_ms(sink.addr).await);
        tokio::time::sleep(Duration::from_millis(200)).await;
    }
    eprintln!("idle handshake: {base:.1} ms");
    Rig {
        sink,
        throttle,
        enabled,
        shared,
        cancel,
        base,
    }
}

struct Phase {
    delay_ms: Vec<f64>,
    modes: Vec<Mode>,
    bytes: u64,
    secs: f64,
}

impl Phase {
    fn mean_delay(&self) -> f64 {
        self.delay_ms.iter().sum::<f64>() / self.delay_ms.len() as f64
    }
    fn p90_delay(&self) -> f64 {
        let mut v = self.delay_ms.clone();
        v.sort_by(f64::total_cmp);
        v[(v.len() - 1) * 9 / 10]
    }
    /// Our upload rate, bytes/s.
    fn throughput(&self) -> f64 {
        self.bytes as f64 / self.secs
    }
    fn seen(&self, m: Mode) -> bool {
        self.modes.contains(&m)
    }
}

impl Rig {
    /// Measures for `secs`: what other traffic waits, the controller's mode, our throughput.
    async fn measure(&self, secs: u64, label: &str) -> Phase {
        let start = Instant::now();
        let bytes_start = self.sink.ours.load(Ordering::Relaxed);
        let (mut delay_ms, mut modes) = (Vec::new(), Vec::new());
        let mut last_print = Instant::now();
        while start.elapsed() < Duration::from_secs(secs) {
            delay_ms.push(handshake_ms(self.sink.addr).await - self.base);
            let s = *self.shared.lock().unwrap();
            modes.push(s.mode);
            if last_print.elapsed() >= Duration::from_secs(2) {
                eprintln!(
                    "{label} {:5.1}s: other traffic waits {:5.0} ms, {:?} limit {:?} KB/s, \
                     base {:?} delay {:?}, socket backlog {} KB, ours {:.0} KB/s",
                    start.elapsed().as_secs_f64(),
                    delay_ms.last().unwrap(),
                    s.mode,
                    s.limit.map(|l| l / 1024),
                    s.base_ms.map(|b| b.round()),
                    s.delay_ms.map(|d| d.round()),
                    socket_backlog(self.sink.addr.port()) / 1024,
                    (self.sink.ours.load(Ordering::Relaxed) - bytes_start) as f64
                        / 1024.0
                        / start.elapsed().as_secs_f64()
                );
                last_print = Instant::now();
            }
            tokio::time::sleep(Duration::from_millis(250)).await;
        }
        Phase {
            delay_ms,
            modes,
            bytes: self.sink.ours.load(Ordering::Relaxed) - bytes_start,
            secs: start.elapsed().as_secs_f64(),
        }
    }

    fn report(&self, name: &str, off: &Phase, on: &Phase) {
        eprintln!(
            "{name}\n  unhindered: other traffic waits {:.0} ms (p90 {:.0}), ours {:.0} KB/s\n  \
             automatic:  other traffic waits {:.0} ms (p90 {:.0}), ours {:.0} KB/s, limited {}",
            off.mean_delay(),
            off.p90_delay(),
            off.throughput() / 1024.0,
            on.mean_delay(),
            on.p90_delay(),
            on.throughput() / 1024.0,
            on.seen(Mode::Limited)
        );
    }
}

/// Our own upload fills a bloated link: the controller must shorten the queue a lot at
/// little cost in speed.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
#[ignore]
async fn netem_bloated_link_gets_a_short_queue() {
    if !shaped("bloated") {
        return;
    }
    let r = rig().await;
    for _ in 0..2 {
        bulk_upload(r.sink.addr, Some(r.throttle.clone()), r.cancel.clone());
    }
    tokio::time::sleep(Duration::from_secs(5)).await;
    let off = r.measure(30, "off").await;
    r.enabled.store(true, Ordering::Relaxed);
    let _settle = r.measure(25, "settling").await;
    let on = r.measure(45, "on").await;
    r.cancel.cancel();
    r.report("bloated link, our upload", &off, &on);

    assert!(
        off.mean_delay() > 300.0,
        "the link isn't bloated ({:.0} ms): is the tc qdisc in place?",
        off.mean_delay()
    );
    assert!(
        on.mean_delay() <= 0.25 * off.mean_delay(),
        "queue not shortened enough: {:.0} ms vs {:.0} ms unhindered",
        on.mean_delay(),
        off.mean_delay()
    );
    assert!(
        on.p90_delay() <= 0.5 * off.p90_delay(),
        "queue spikes too high: p90 {:.0} ms vs {:.0} ms unhindered",
        on.p90_delay(),
        off.p90_delay()
    );
    assert!(
        on.throughput() >= 0.85 * off.throughput(),
        "too slow: {:.0} KB/s vs {:.0} KB/s unhindered",
        on.throughput() / 1024.0,
        off.throughput() / 1024.0
    );
}

/// The same speed behind a short buffer: nothing to fix, so never limited and no loss.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
#[ignore]
async fn netem_healthy_link_is_never_slowed() {
    if !shaped("healthy") {
        return;
    }
    let r = rig().await;
    for _ in 0..2 {
        bulk_upload(r.sink.addr, Some(r.throttle.clone()), r.cancel.clone());
    }
    tokio::time::sleep(Duration::from_secs(5)).await;
    let off = r.measure(25, "off").await;
    r.enabled.store(true, Ordering::Relaxed);
    // Switched on mid-upload: at most one calibration pause (~6 s), then nothing.
    let _settle = r.measure(15, "settling").await;
    let on = r.measure(40, "on").await;
    r.cancel.cancel();
    r.report("healthy link, our upload", &off, &on);

    assert!(
        off.mean_delay() < 100.0,
        "the link is bloated ({:.0} ms): is the healthy tc qdisc in place?",
        off.mean_delay()
    );
    assert!(!on.seen(Mode::Limited), "limited on a healthy link");
    assert!(!on.seen(Mode::Checking), "paused again after settling");
    assert!(
        on.throughput() >= 0.97 * off.throughput(),
        "slower on a healthy link: {:.0} KB/s vs {:.0} KB/s",
        on.throughput() / 1024.0,
        off.throughput() / 1024.0
    );
}

/// Someone else's unpaced upload keeps the bloated queue full: pausing ours changes
/// nothing, so ours must never be limited.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
#[ignore]
async fn netem_someone_elses_upload_does_not_slow_ours() {
    if !shaped("bloated") {
        return;
    }
    let r = rig().await;
    bulk_upload(r.sink.addr, None, r.cancel.clone());
    bulk_upload(r.sink.addr, None, r.cancel.clone());
    bulk_upload(r.sink.addr, Some(r.throttle.clone()), r.cancel.clone());
    tokio::time::sleep(Duration::from_secs(5)).await;
    let off = r.measure(30, "off").await;
    r.enabled.store(true, Ordering::Relaxed);
    let on = r.measure(60, "on").await;
    r.cancel.cancel();
    r.report("bloated link, someone else's upload", &off, &on);

    assert!(off.mean_delay() > 300.0, "the link isn't bloated");
    assert!(
        !on.seen(Mode::Limited),
        "limited for a queue that isn't ours"
    );
    // Calibration or a check may pause ours once (≤ ~8 s of 60).
    assert!(
        on.throughput() >= 0.8 * off.throughput(),
        "ours slowed: {:.0} KB/s vs {:.0} KB/s",
        on.throughput() / 1024.0,
        off.throughput() / 1024.0
    );
}
