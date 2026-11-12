//! Tests for the automatic upload speed controller: unit tests of each rule, then a
//! simulated network (bottleneck link with a buffer, the OS socket buffer, the token-bucket
//! throttle, other traffic, delay from elsewhere, measurement noise) that checks the two
//! promises: no slowdown where there is no problem, a short queue at near-full speed where
//! there is one.

use super::*;

const KB: f64 = 1024.0;
const MB: f64 = 1024.0 * 1024.0;

// ----- Unit tests: drive the controller by hand.

/// Feeds samples every `step` ms for `secs`, uploading at `rate` bytes/s (0 = idle), with
/// round trips from `rtt(t, decision)`. Returns the decisions seen.
struct Driver {
    ctl: Controller,
    now: u64,
    passed: u64,
    decision: Decision,
    /// The rate uploads want during `run`.
    want: f64,
}

impl Driver {
    fn new() -> Self {
        Self::with(Params::default())
    }

    fn with(p: Params) -> Self {
        Self {
            ctl: Controller::new(p),
            now: 0,
            passed: 0,
            decision: Decision::default(),
            want: 0.0,
        }
    }

    /// Idle samples to learn the base.
    fn learn_base(&mut self, rtt: f64) {
        for _ in 0..5 {
            self.step(500, 0.0, Some(rtt));
        }
    }

    fn step(&mut self, ms: u64, rate: f64, rtt: Option<f64>) -> Decision {
        self.now += ms;
        if !self.decision.hold {
            let mut r = rate;
            if let Some(l) = self.decision.limit {
                r = r.min(l as f64);
            }
            self.passed += (r * ms as f64 / 1000.0) as u64;
        }
        if let Some(rtt) = rtt {
            self.ctl.on_sample(self.now, rtt);
        }
        self.decision = self.ctl.tick(self.now, self.passed, rate > 0.0);
        self.decision
    }

    /// Runs for `secs` at `rate` with round trips from `rtt(self)`.
    fn run(&mut self, secs: u64, rate: f64, mut rtt: impl FnMut(&Self) -> Option<f64>) {
        let end = self.now + secs * 1000;
        self.want = rate;
        while self.now < end {
            let ms = self.ctl.interval(self.now).as_millis() as u64;
            let r = rtt(self);
            self.step(ms, rate, r);
        }
    }

    fn mode(&self) -> Mode {
        self.ctl.mode()
    }
}

#[test]
fn no_measurements_never_limit() {
    let mut d = Driver::new();
    d.run(600, 5.0 * MB, |_| None);
    assert_eq!(d.mode(), Mode::Unlimited);
    assert_eq!(d.decision, Decision::default());
}

#[test]
fn base_is_the_lowest_round_trip_and_ages_out() {
    let mut c = Controller::new(Params::default());
    c.on_sample(0, 50.0);
    c.on_sample(1_000, 30.0);
    assert_eq!(c.base_ms(), None, "needs three samples first");
    c.on_sample(2_000, 80.0);
    assert_eq!(c.base_ms(), Some(30.0));
    // Ten minutes later the old minimum is gone (the network may have changed).
    c.on_sample(11 * 60_000, 90.0);
    c.on_sample(11 * 60_000 + 500, 95.0);
    c.on_sample(11 * 60_000 + 900, 92.0);
    assert_eq!(c.base_ms(), Some(90.0));
}

#[test]
fn spikes_are_filtered() {
    let mut d = Driver::new();
    d.learn_base(20.0);
    // Every third sample spikes to 500 ms while uploading: the filter (lowest of ~3 s) never
    // sees a sustained delay.
    let mut i = 0;
    d.run(300, 5.0 * MB, |_| {
        i += 1;
        Some(if i % 3 == 0 { 500.0 } else { 25.0 })
    });
    assert_eq!(d.mode(), Mode::Unlimited);
}

#[test]
fn high_delay_while_idle_does_nothing() {
    let mut d = Driver::new();
    d.learn_base(20.0);
    d.run(300, 0.0, |_| Some(600.0));
    assert_eq!(d.mode(), Mode::Unlimited);
    // Uploading slower than the minimum rate can't be the cause either.
    d.run(300, 10.0 * KB, |_| Some(600.0));
    assert_eq!(d.mode(), Mode::Unlimited);
}

#[test]
fn a_short_high_delay_does_not_start_a_check() {
    let mut d = Driver::new();
    d.learn_base(20.0);
    d.run(20, 5.0 * MB, |_| Some(25.0));
    // 4 s of high delay (less than engage_for_ms = 6 s).
    d.run(4, 5.0 * MB, |_| Some(400.0));
    d.run(20, 5.0 * MB, |_| Some(25.0));
    assert_eq!(d.mode(), Mode::Unlimited);
}

/// Round trip of a simple bloated link: high while we upload unhindered, low when paused or
/// limited below `cap`.
fn our_queue(cap: f64) -> impl FnMut(&Driver) -> Option<f64> {
    move |d: &Driver| {
        let sending = match (d.decision.hold, d.decision.limit) {
            (true, _) => 0.0,
            (false, Some(l)) => d.want.min(l as f64),
            (false, None) => d.want,
        };
        Some(if sending > cap { 520.0 } else { 22.0 })
    }
}

#[test]
fn our_own_queue_is_detected_and_limited() {
    let mut d = Driver::new();
    d.learn_base(20.0);
    d.run(5, 2.0 * MB, our_queue(1.0 * MB));
    assert_eq!(d.mode(), Mode::Unlimited, "not before engage_for_ms");
    d.run(5, 2.0 * MB, our_queue(1.0 * MB));
    // Paused for the check: the delay falls, so a limit starts below the rate before.
    d.run(4, 2.0 * MB, our_queue(1.0 * MB));
    assert_eq!(d.mode(), Mode::Limited);
    // Then the limit settles around the link's 1 MB/s.
    let mut seen = Vec::new();
    d.run(120, 2.0 * MB, |d| {
        seen.push(d.decision.limit.unwrap_or(0) as f64);
        our_queue(1.0 * MB)(d)
    });
    assert_eq!(d.mode(), Mode::Limited);
    let late = &seen[seen.len() / 2..];
    let avg = late.iter().sum::<f64>() / late.len() as f64;
    assert!(avg > 0.7 * MB && avg < 1.3 * MB, "average limit {avg}");
}

#[test]
fn a_delay_that_is_not_ours_leaves_uploads_alone() {
    let mut d = Driver::new();
    d.learn_base(20.0);
    // 400 ms extra whatever we do (someone else's traffic, a VPN, a full download link).
    let mut holds = 0u64;
    let end = d.now + 20 * 60_000;
    while d.now < end {
        let ms = d.ctl.interval(d.now).as_millis() as u64;
        let dec = d.step(ms, 3.0 * MB, Some(420.0));
        if dec.hold {
            holds += ms;
        }
        assert!(dec.limit.is_none(), "limited at {} ms", d.now);
    }
    // One check, then one per cooldown: ≤ 5 checks of ≤ 8 s in 20 minutes.
    assert!(holds <= 5 * 8_250, "paused {holds} ms");
    assert!(holds > 0, "it did check");
}

#[test]
fn no_check_during_the_cooldown() {
    let mut d = Driver::new();
    d.learn_base(20.0);
    d.run(30, 3.0 * MB, |_| Some(420.0)); // first check fails
    let mut holds = 0;
    d.run(240, 3.0 * MB, |d| {
        if d.decision.hold {
            holds += 1;
        }
        Some(420.0)
    });
    assert_eq!(holds, 0, "a new check within the 5-minute cooldown");
}

#[test]
fn the_limit_is_removed_when_uploads_end() {
    let mut d = Driver::new();
    d.learn_base(20.0);
    d.run(30, 2.0 * MB, our_queue(1.0 * MB));
    assert_eq!(d.mode(), Mode::Limited);
    d.run(40, 0.0, |_| Some(21.0));
    assert_eq!(d.mode(), Mode::Unlimited);
    assert_eq!(d.decision, Decision::default());
}

#[test]
fn the_limit_is_removed_when_measurements_stop() {
    let mut d = Driver::new();
    d.learn_base(20.0);
    d.run(30, 2.0 * MB, our_queue(1.0 * MB));
    assert_eq!(d.mode(), Mode::Limited);
    d.run(8, 2.0 * MB, |_| None);
    assert_eq!(d.mode(), Mode::Unlimited, "fail open");
}

#[test]
fn the_limit_is_removed_when_it_stops_mattering() {
    let mut d = Driver::new();
    d.learn_base(20.0);
    d.run(30, 2.0 * MB, our_queue(1.0 * MB));
    assert_eq!(d.mode(), Mode::Limited);
    // The uploads now only need 100 KB/s (small files) and the queue is empty.
    d.run(40, 100.0 * KB, |_| Some(21.0));
    assert_eq!(d.mode(), Mode::Unlimited);
}

#[test]
fn the_limit_is_removed_when_the_link_gets_faster() {
    let mut d = Driver::new();
    d.learn_base(20.0);
    d.run(30, 2.0 * MB, our_queue(1.0 * MB));
    assert_eq!(d.mode(), Mode::Limited);
    // The link is now 10x faster (and uploads want 8 MB/s): the queue stays empty, uploads
    // soon run faster than ever before and the limit is removed.
    d.ctl.take_events();
    d.run(60, 8.0 * MB, our_queue(10.0 * MB));
    assert_eq!(d.mode(), Mode::Unlimited);
    let ev = d.ctl.take_events().join("; ");
    assert!(ev.contains("limit removed"), "{ev}");
    // (Wanting more than the faster link carries would rightly be limited again, at the new
    // speed: see trace_faster.)
}

#[test]
fn stuck_at_the_floor_checks_again() {
    let mut d = Driver::new();
    d.learn_base(20.0);
    d.run(30, 2.0 * MB, our_queue(1.0 * MB));
    assert_eq!(d.mode(), Mode::Limited);
    // Now the delay stays high whatever we do (another device started a big upload).
    let mut checked = false;
    let mut released = false;
    d.run(180, 2.0 * MB, |d| {
        checked |= d.decision.hold;
        released |= checked && d.mode() == Mode::Unlimited;
        Some(520.0)
    });
    assert!(
        checked && released,
        "checked {checked}, released {released}"
    );
    assert_eq!(d.mode(), Mode::Unlimited);
}

#[test]
fn calibration_without_a_queue_is_trusted_for_half_an_hour() {
    let mut d = Driver::new();
    // Uploading from the very first sample: the base is unverified. One calibration, then
    // none for 30 minutes (50 minutes: the second at ~30 min).
    let mut holds = 0u64;
    let mut pauses = 0;
    let mut was_holding = false;
    let end = 3_000_000; // 50 min
    while d.now < end {
        let ms = d.ctl.interval(d.now).as_millis() as u64;
        let dec = d.step(ms, 3.0 * MB, Some(25.0));
        if dec.hold {
            holds += ms;
            if !was_holding {
                pauses += 1;
            }
        }
        was_holding = dec.hold;
        assert!(dec.limit.is_none());
    }
    assert_eq!(pauses, 2, "calibrations: {pauses}");
    // Each: the 6 s abort plus the last measurement round.
    assert!(holds <= 14_000, "paused {holds} ms");
}

#[test]
fn reset_removes_everything() {
    let mut d = Driver::new();
    d.learn_base(20.0);
    d.run(30, 2.0 * MB, our_queue(1.0 * MB));
    assert_eq!(d.mode(), Mode::Limited);
    d.ctl.reset();
    assert_eq!(d.ctl.decision(), Decision::default());
}

#[test]
fn garbage_samples_are_ignored() {
    let mut c = Controller::new(Params::default());
    c.on_sample(0, f64::NAN);
    c.on_sample(1, -5.0);
    c.on_sample(2, f64::INFINITY);
    assert_eq!(c.base_ms(), None);
    // Time going backwards doesn't panic.
    c.tick(10_000, 100, true);
    c.tick(5_000, 50, false);
    c.tick(6_000, 10, true);
}

// ----- Simulated network.

/// xorshift64*: deterministic noise without extra dependencies.
struct Rng(u64);

impl Rng {
    fn unit(&mut self) -> f64 {
        self.0 ^= self.0 >> 12;
        self.0 ^= self.0 << 25;
        self.0 ^= self.0 >> 27;
        (self.0.wrapping_mul(0x2545_f491_4f6c_dd1d) >> 11) as f64 / (1u64 << 53) as f64
    }
    fn exp(&mut self, mean: f64) -> f64 {
        -mean * (1.0 - self.unit()).ln()
    }
}

type Curve = Box<dyn Fn(f64) -> f64>;
type Noise = Box<dyn Fn(&mut Rng, f64) -> f64>;

struct Scenario {
    /// Bottleneck rate, bytes/s, over time.
    cap: Curve,
    /// Bottleneck buffer, in seconds of the (initial) rate.
    buffer_s: f64,
    base_ms: f64,
    /// OS socket send buffer: bytes our upload has handed over but not yet sent.
    sockbuf: f64,
    /// Our upload's own maximum rate (`INFINITY` = bulk upload).
    demand: Curve,
    /// Other traffic into the same queue, bytes/s.
    other: Curve,
    /// Delay from outside this queue (download link, VPN), ms.
    extra_ms: Curve,
    /// Measurement noise, ms (≥ 0: noise only ever adds delay).
    noise: Noise,
    /// Probes fail (no answer) at these times.
    probe_fails: Box<dyn Fn(f64) -> bool>,
    /// Uploads start after this many seconds (the service measures while idle first).
    start_s: f64,
    /// Measuring starts at this time (later than `start_s`: switched on mid-upload).
    enable_at: f64,
    secs: f64,
    seed: u64,
    controlled: bool,
}

impl Scenario {
    /// 10 Mbit/s up with a 1 s buffer: a typical bloated cable/DSL uplink.
    fn bloated() -> Self {
        Self {
            cap: Box::new(|_| 1.25 * MB),
            buffer_s: 1.0,
            base_ms: 25.0,
            sockbuf: 512.0 * KB,
            demand: Box::new(|_| f64::INFINITY),
            other: Box::new(|_| 0.0),
            extra_ms: Box::new(|_| 0.0),
            noise: Box::new(|r, _| r.exp(2.0)),
            probe_fails: Box::new(|_| false),
            start_s: 10.0,
            enable_at: 0.0,
            secs: 600.0,
            seed: 7,
            controlled: true,
        }
    }

    /// Same link behind a router with active queue management (fq_codel/SQM, many modern
    /// routers and fibre): the queue never builds beyond ~15 ms.
    fn managed() -> Self {
        Self {
            buffer_s: 0.015,
            ..Self::bloated()
        }
    }

    fn uncontrolled(mut self) -> Self {
        self.controlled = false;
        self
    }
}

#[derive(Clone, Copy)]
struct Rec {
    t: f64,
    mode: Mode,
    /// Delay another packet would see now (queue + extra), ms above the base.
    delay_ms: f64,
    /// Our bytes delivered in this 100 ms slice.
    ours: f64,
}

struct Outcome {
    recs: Vec<Rec>,
    events: Vec<(f64, String)>,
    checks: usize,
}

impl Outcome {
    fn window(&self, from: f64, to: f64) -> impl Iterator<Item = &Rec> {
        self.recs.iter().filter(move |r| r.t >= from && r.t < to)
    }
    /// Our throughput, bytes/s.
    fn throughput(&self, from: f64, to: f64) -> f64 {
        let n = self.window(from, to).count() as f64;
        self.window(from, to).map(|r| r.ours).sum::<f64>() / (n * 0.1)
    }
    fn delay_mean(&self, from: f64, to: f64) -> f64 {
        let n = self.window(from, to).count() as f64;
        self.window(from, to).map(|r| r.delay_ms).sum::<f64>() / n
    }
    fn delay_p(&self, from: f64, to: f64, p: f64) -> f64 {
        let mut v: Vec<f64> = self.window(from, to).map(|r| r.delay_ms).collect();
        v.sort_by(f64::total_cmp);
        v[((v.len() - 1) as f64 * p) as usize]
    }
    fn time_in(&self, mode: Mode, from: f64, to: f64) -> f64 {
        self.window(from, to).filter(|r| r.mode == mode).count() as f64 * 0.1
    }
    fn dump(&self) -> String {
        let mut s = String::new();
        for (t, e) in &self.events {
            s.push_str(&format!("  {t:7.1}s {e}\n"));
        }
        s
    }
}

fn simulate(sc: &Scenario) -> Outcome {
    let dt = 0.005;
    let mut rng = Rng(sc.seed.max(1));
    let mut ctl = Controller::new(Params::default());
    let buffer = sc.buffer_s * (sc.cap)(0.0);
    let (mut q_ours, mut q_other, mut sock, mut tokens) = (0.0f64, 0.0f64, 0.0f64, 0.0f64);
    let mut passed = 0.0f64;
    let mut decision = Decision::default();
    // Probe in flight: (arrives at, measured round trip or None for a failure).
    let mut pending: Option<(f64, Option<f64>)> = None;
    let mut probe_sent = 0.0;
    let mut next_probe = 0.0;
    let mut recs = Vec::new();
    let mut events = Vec::new();
    let mut checks = 0;
    let (mut slice_ours, mut slice_end) = (0.0, 0.1);
    let mut t = 0.0;
    let ms = |t: f64| (t * 1000.0).round() as u64;
    while t < sc.secs {
        let cap = (sc.cap)(t);
        if sc.controlled && t >= sc.enable_at {
            if let Some((arrive, rtt)) = pending
                && t >= arrive
            {
                if let Some(r) = rtt {
                    ctl.on_sample(ms(t), r);
                }
                let before = ctl.mode();
                decision = ctl.tick(
                    ms(t),
                    passed as u64,
                    t >= sc.start_s && (sc.demand)(t) > 0.0,
                );
                if before != Mode::Checking && ctl.mode() == Mode::Checking {
                    checks += 1;
                }
                for e in ctl.take_events() {
                    events.push((t, e));
                }
                pending = None;
                next_probe = (probe_sent + ctl.interval(ms(t)).as_secs_f64()).max(t);
            }
            if pending.is_none() && t >= next_probe {
                probe_sent = t;
                pending = Some(if (sc.probe_fails)(t) {
                    (t + 0.05, None)
                } else {
                    let rtt = sc.base_ms
                        + (q_ours + q_other) / cap * 1000.0
                        + (sc.extra_ms)(t)
                        + (sc.noise)(&mut rng, t);
                    let rtt = rtt.min(3000.0);
                    (t + rtt / 1000.0, Some(rtt))
                });
            }
        }
        // Our upload writes into the socket buffer through the throttle.
        if t >= sc.start_s && !decision.hold {
            let mut w = (sc.sockbuf - sock).max(0.0).min((sc.demand)(t) * dt);
            match decision.limit {
                Some(l) => {
                    let l = l as f64;
                    tokens = (tokens + l * dt).min(l);
                    w = w.min(tokens.max(0.0));
                    tokens -= w;
                }
                None => tokens = 0.0,
            }
            sock += w;
            passed += w;
        }
        // TCP moves data from the socket buffer into the bottleneck queue while it has room
        // (loss-based TCP keeps a bloated buffer full).
        let room = (buffer - q_ours - q_other).max(0.0);
        let send = sock.min(room).min(cap * 3.0 * dt);
        sock -= send;
        q_ours += send;
        let room = (buffer - q_ours - q_other).max(0.0);
        q_other += ((sc.other)(t) * dt).min(room);
        // The link serves the queue.
        let total = q_ours + q_other;
        if total > 0.0 {
            let f = (cap * dt / total).min(1.0);
            let ours = q_ours * f;
            q_ours -= ours;
            q_other -= q_other * f;
            slice_ours += ours;
        }
        t += dt;
        if t >= slice_end {
            recs.push(Rec {
                t: slice_end - 0.1,
                mode: ctl.mode(),
                delay_ms: (q_ours + q_other) / cap * 1000.0 + (sc.extra_ms)(t),
                ours: slice_ours,
            });
            slice_ours = 0.0;
            slice_end += 0.1;
        }
    }
    Outcome {
        recs,
        events,
        checks,
    }
}

// --- Where the controller must not get in the way.

#[test]
fn sim_managed_queue_is_never_slowed() {
    let out = simulate(&Scenario::managed());
    let base = simulate(&Scenario::managed().uncontrolled());
    assert_eq!(out.checks, 0, "{}", out.dump());
    assert!(out.events.is_empty(), "{}", out.dump());
    let (a, b) = (out.throughput(20.0, 600.0), base.throughput(20.0, 600.0));
    assert!(a >= 0.999 * b, "{a} vs {b}");
    assert!(a >= 0.95 * 1.25 * MB, "{a}");
}

#[test]
fn sim_wifi_jitter_is_never_slowed() {
    // Wi-Fi: 3–30 ms of jitter on every packet, a 300 ms spike on 3 % of probes, and a
    // 2-second interference burst (+150 ms) every few minutes.
    let mut sc = Scenario::managed();
    sc.secs = 1800.0;
    sc.noise = Box::new(|r, t| {
        let burst = if (t % 170.0) < 2.0 { 150.0 } else { 0.0 };
        let spike = if r.unit() < 0.03 { 300.0 } else { 0.0 };
        3.0 + r.exp(10.0) + burst + spike
    });
    let out = simulate(&sc);
    let base = simulate(&{
        let mut b = Scenario::managed();
        b.secs = 1800.0;
        b.controlled = false;
        b
    });
    assert_eq!(
        out.time_in(Mode::Limited, 0.0, 1800.0),
        0.0,
        "{}",
        out.dump()
    );
    let (a, b) = (out.throughput(20.0, 1800.0), base.throughput(20.0, 1800.0));
    assert!(a >= 0.99 * b, "{a} vs {b}\n{}", out.dump());
}

#[test]
fn sim_someone_elses_upload_does_not_slow_ours() {
    // Another device uploads flat out into the same bloated queue from the start.
    let mut sc = Scenario::bloated();
    sc.other = Box::new(|_| 1.25 * MB);
    let out = simulate(&sc);
    let base = simulate(&{
        let mut b = Scenario::bloated();
        b.other = Box::new(|_| 1.25 * MB);
        b.controlled = false;
        b
    });
    assert_eq!(
        out.time_in(Mode::Limited, 0.0, 600.0),
        0.0,
        "{}",
        out.dump()
    );
    let (a, b) = (out.throughput(20.0, 600.0), base.throughput(20.0, 600.0));
    // Only the checks (≤ 8 s every 5 min) may cost anything.
    assert!(a >= 0.95 * b, "{a} vs {b}\n{}", out.dump());
}

#[test]
fn sim_a_busy_download_link_does_not_slow_uploads() {
    // A big download fills the *downstream* queue: handshakes are slow, but pausing our
    // upload changes nothing.
    let mut sc = Scenario::managed();
    sc.extra_ms = Box::new(|t| if t > 60.0 { 350.0 } else { 0.0 });
    let out = simulate(&sc);
    assert_eq!(
        out.time_in(Mode::Limited, 0.0, 600.0),
        0.0,
        "{}",
        out.dump()
    );
    assert!(out.checks <= 3, "{} checks\n{}", out.checks, out.dump());
    let a = out.throughput(20.0, 600.0);
    assert!(a >= 0.93 * 1.25 * MB, "{a}\n{}", out.dump());
}

#[test]
fn sim_a_vpn_turning_on_does_not_slow_uploads() {
    // The base round trip jumps by 120 ms for good (a VPN, another route). After at most a
    // couple of checks the new base is learned (10-minute window).
    let mut sc = Scenario::managed();
    sc.secs = 1800.0;
    sc.extra_ms = Box::new(|t| if t > 120.0 { 120.0 } else { 0.0 });
    let out = simulate(&sc);
    assert_eq!(
        out.time_in(Mode::Limited, 0.0, 1800.0),
        0.0,
        "{}",
        out.dump()
    );
    assert!(out.checks <= 3, "{} checks\n{}", out.checks, out.dump());
    assert!(out.throughput(20.0, 1800.0) >= 0.97 * 1.25 * MB);
}

#[test]
fn sim_small_uploads_are_never_slowed() {
    // A trickle of small files: 40 KB/s on a bloated link.
    let mut sc = Scenario::bloated();
    sc.demand = Box::new(|_| 40.0 * KB);
    let out = simulate(&sc);
    assert_eq!(out.checks, 0, "{}", out.dump());
    let a = out.throughput(20.0, 600.0);
    assert!(a >= 0.97 * 40.0 * KB, "{a}");
}

#[test]
fn sim_failing_measurements_never_slow_uploads() {
    let mut sc = Scenario::bloated();
    sc.probe_fails = Box::new(|_| true);
    let out = simulate(&sc);
    assert_eq!(out.checks, 0);
    assert!(out.throughput(20.0, 600.0) >= 0.97 * 1.25 * MB);
}

#[test]
fn sim_restarted_mid_upload_still_finds_the_queue() {
    // The service starts while uploads resume at once: every first sample sits behind a
    // full queue, so the base learned from them is too high. A calibration pause finds the
    // real base and the queue.
    let mut sc = Scenario::bloated();
    sc.start_s = 0.0;
    let out = simulate(&sc);
    assert!(
        out.time_in(Mode::Limited, 0.0, 60.0) > 0.0,
        "{}",
        out.dump()
    );
    assert!(out.delay_mean(120.0, 600.0) <= 150.0, "{}", out.dump());
    assert!(
        out.throughput(120.0, 600.0) >= 0.85 * 1.25 * MB,
        "{}",
        out.dump()
    );
}

#[test]
fn sim_switched_on_mid_upload_still_finds_the_queue() {
    // No measurements at all until 60 s, uploads running since 10 s (the real-network test
    // does this too).
    let mut sc = Scenario::bloated();
    sc.enable_at = 60.0;
    let out = simulate(&sc);
    assert!(
        out.time_in(Mode::Limited, 60.0, 120.0) > 0.0,
        "{}",
        out.dump()
    );
    assert!(out.delay_mean(150.0, 600.0) <= 150.0, "{}", out.dump());
    assert!(
        out.throughput(150.0, 600.0) >= 0.85 * 1.25 * MB,
        "{}",
        out.dump()
    );
    // Once engaged it stays engaged: no release-and-check-again cycles (the real-network
    // test once caught a release rule misfiring while uploads ran at the link's speed).
    let (a, b) = (120.0, 600.0);
    let limited = out.time_in(Mode::Limited, a, b);
    assert!(
        limited >= 0.98 * (b - a),
        "limited only {limited} s of {}\n{}",
        b - a,
        out.dump()
    );
    assert!(out.checks <= 2, "{} checks\n{}", out.checks, out.dump());
}

#[test]
fn sim_restarted_mid_upload_on_a_healthy_link_costs_one_short_pause() {
    let mut sc = Scenario::managed();
    sc.start_s = 0.0;
    sc.secs = 1800.0;
    let out = simulate(&sc);
    let base = simulate(&{
        let mut b = Scenario::managed();
        b.start_s = 0.0;
        b.secs = 1800.0;
        b.controlled = false;
        b
    });
    assert_eq!(
        out.time_in(Mode::Limited, 0.0, 1800.0),
        0.0,
        "{}",
        out.dump()
    );
    let paused = out.time_in(Mode::Checking, 0.0, 1800.0);
    assert!(paused <= 6.5, "paused {paused} s\n{}", out.dump());
    let (a, b) = (out.throughput(0.0, 1800.0), base.throughput(0.0, 1800.0));
    assert!(a >= 0.99 * b, "{a} vs {b}\n{}", out.dump());
}

#[test]
fn sim_long_upload_does_not_drift() {
    // 40 minutes on a bloated link: the base must not creep up while we regulate.
    let mut sc = Scenario::bloated();
    sc.secs = 2400.0;
    let out = simulate(&sc);
    for (from, to) in [
        (120.0, 600.0),
        (600.0, 1200.0),
        (1200.0, 1800.0),
        (1800.0, 2400.0),
    ] {
        let (d, tp) = (out.delay_mean(from, to), out.throughput(from, to));
        assert!(d <= 150.0, "{from}-{to} s: delay {d}\n{}", out.dump());
        assert!(
            tp >= 0.85 * 1.25 * MB,
            "{from}-{to} s: throughput {tp}\n{}",
            out.dump()
        );
    }
}

// --- Where it must help, without costing much speed.

#[test]
fn sim_bloated_uplink_gets_a_short_queue_at_near_full_speed() {
    let out = simulate(&Scenario::bloated());
    let base = simulate(&Scenario::bloated().uncontrolled());
    // Sanity: unhindered, the link is ~1 s deep.
    assert!(base.delay_mean(30.0, 600.0) > 800.0);
    // Engaged within the first minute.
    assert!(
        out.time_in(Mode::Limited, 0.0, 60.0) > 0.0,
        "{}",
        out.dump()
    );
    // After settling: queue short, speed close to the link's.
    let mean = out.delay_mean(90.0, 600.0);
    let p95 = out.delay_p(90.0, 600.0, 0.95);
    let tp = out.throughput(90.0, 600.0);
    eprintln!(
        "bloated: delay mean {mean:.0} ms, p95 {p95:.0} ms, throughput {:.0}% of the link\n{}",
        100.0 * tp / (1.25 * MB),
        out.dump()
    );
    assert!(mean <= 80.0, "mean delay {mean}\n{}", out.dump());
    assert!(p95 <= 200.0, "p95 delay {p95}\n{}", out.dump());
    assert!(tp >= 0.85 * 1.25 * MB, "throughput {tp}\n{}", out.dump());
    // Once engaged it stays engaged: no release-and-check-again cycles (the real-network
    // test once caught a release rule misfiring while uploads ran at the link's speed).
    let (a, b) = (60.0, 600.0);
    let limited = out.time_in(Mode::Limited, a, b);
    assert!(
        limited >= 0.98 * (b - a),
        "limited only {limited} s of {}\n{}",
        b - a,
        out.dump()
    );
    assert!(out.checks <= 2, "{} checks\n{}", out.checks, out.dump());
}

#[test]
fn sim_large_socket_buffers_still_pass_the_check() {
    // Windows can buffer several MB per connection: the check must still see the drain.
    let mut sc = Scenario::bloated();
    sc.sockbuf = 4.0 * MB;
    let out = simulate(&sc);
    assert!(
        out.time_in(Mode::Limited, 0.0, 120.0) > 0.0,
        "{}",
        out.dump()
    );
    assert!(out.delay_mean(120.0, 600.0) <= 150.0, "{}", out.dump());
    assert!(
        out.throughput(120.0, 600.0) >= 0.8 * 1.25 * MB,
        "{}",
        out.dump()
    );
}

#[test]
fn sim_follows_the_link_speed_down_and_up() {
    // 10 Mbit/s, then 3 Mbit/s from 200 s, then 25 Mbit/s from 400 s.
    let mut sc = Scenario::bloated();
    sc.secs = 700.0;
    sc.cap = Box::new(|t| {
        if t < 200.0 {
            1.25 * MB
        } else if t < 400.0 {
            0.375 * MB
        } else {
            3.125 * MB
        }
    });
    let out = simulate(&sc);
    eprintln!("{}", out.dump());
    // Slow phase: the queue comes back down within ~40 s of the drop.
    assert!(out.delay_mean(240.0, 400.0) <= 150.0, "{}", out.dump());
    assert!(
        out.throughput(240.0, 400.0) >= 0.8 * 0.375 * MB,
        "{}",
        out.dump()
    );
    // Fast phase: within a minute uploads use most of the faster link (limited or not).
    let fast = out.throughput(460.0, 700.0);
    assert!(
        fast >= 0.8 * 3.125 * MB,
        "fast phase {fast}\n{}",
        out.dump()
    );
}

#[test]
fn sim_shares_with_a_video_call() {
    // A 2 Mbit/s video call alongside our upload on a bloated 10 Mbit/s link: the call's
    // packets must not wait behind a second of our data.
    let mut sc = Scenario::bloated();
    sc.other = Box::new(|_| 0.25 * MB);
    let out = simulate(&sc);
    let base = simulate(&{
        let mut b = Scenario::bloated();
        b.other = Box::new(|_| 0.25 * MB);
        b.controlled = false;
        b
    });
    assert!(base.delay_mean(60.0, 600.0) > 800.0);
    let mean = out.delay_mean(90.0, 600.0);
    let tp = out.throughput(90.0, 600.0);
    assert!(mean <= 150.0, "{mean}\n{}", out.dump());
    assert!(tp >= 0.75 * 1.0 * MB, "{tp}\n{}", out.dump());
}

#[test]
fn sim_results_hold_across_noise_seeds() {
    for seed in 1..=20 {
        let mut sc = Scenario::bloated();
        sc.seed = seed;
        sc.noise = Box::new(|r, _| 2.0 + r.exp(8.0) + if r.unit() < 0.02 { 250.0 } else { 0.0 });
        let out = simulate(&sc);
        let (mean, tp) = (out.delay_mean(90.0, 600.0), out.throughput(90.0, 600.0));
        assert!(mean <= 150.0, "seed {seed}: delay {mean}\n{}", out.dump());
        assert!(
            tp >= 0.8 * 1.25 * MB,
            "seed {seed}: throughput {tp}\n{}",
            out.dump()
        );

        let mut calm = Scenario::managed();
        calm.seed = seed;
        calm.noise = Box::new(|r, _| 2.0 + r.exp(8.0) + if r.unit() < 0.02 { 250.0 } else { 0.0 });
        let out = simulate(&calm);
        assert_eq!(
            out.time_in(Mode::Limited, 0.0, 600.0),
            0.0,
            "seed {seed}\n{}",
            out.dump()
        );
    }
}

#[test]
#[ignore]
fn trace_bloated() {
    let out = simulate(&Scenario::bloated());
    let mut acc = (0.0f64, 0.0f64, 0usize);
    for r in out.recs.iter().filter(|r| r.t >= 20.0 && r.t < 140.0) {
        acc.0 += r.ours;
        acc.1 = acc.1.max(r.delay_ms);
        acc.2 += 1;
        if acc.2 == 10 {
            eprintln!(
                "{:6.1}s {:?} max delay {:5.0} ms, ours {:4.0} KB/s",
                r.t,
                r.mode,
                acc.1,
                acc.0 / 1024.0
            );
            acc = (0.0, 0.0, 0);
        }
    }
    eprintln!("{}", out.dump());
}

#[test]
#[ignore]
fn trace_faster() {
    let mut d = Driver::new();
    d.learn_base(20.0);
    d.run(30, 2.0 * MB, our_queue(1.0 * MB));
    for i in 0..12 {
        d.run(10, 20.0 * MB, our_queue(10.0 * MB));
        let s = d.ctl.snapshot(d.now);
        eprintln!(
            "{:3}s {:?} limit {:?} rate {:.0} KB/s peak {:.0} KB/s delay {:?}",
            (i + 1) * 10,
            s.mode,
            s.limit.map(|l| l / 1024),
            s.rate / 1024.0,
            d.ctl.peak / 1024.0,
            s.delay_ms
        );
    }
    for e in d.ctl.take_events() {
        eprintln!("{e}");
    }
}

// --- More adversarial scenarios.

#[test]
fn sim_someone_else_starting_while_we_are_limited_releases_the_limit() {
    // We are limited (our own queue); from 200 s another device uploads flat out. Pausing us
    // no longer drains the queue, so the limit must go and stay gone.
    let mut sc = Scenario::bloated();
    sc.other = Box::new(|t| if t > 200.0 { 1.25 * MB } else { 0.0 });
    let out = simulate(&sc);
    assert!(
        out.time_in(Mode::Limited, 60.0, 200.0) > 100.0,
        "{}",
        out.dump()
    );
    // Released within a minute of the other upload starting.
    assert_eq!(
        out.time_in(Mode::Limited, 260.0, 600.0),
        0.0,
        "{}",
        out.dump()
    );
    let base = simulate(&{
        let mut b = Scenario::bloated();
        b.other = Box::new(|t| if t > 200.0 { 1.25 * MB } else { 0.0 });
        b.controlled = false;
        b
    });
    let (a, b) = (out.throughput(260.0, 600.0), base.throughput(260.0, 600.0));
    assert!(a >= 0.95 * b, "{a} vs {b}\n{}", out.dump());
}

#[test]
fn sim_flickering_measurements_do_not_slow_uploads() {
    // Every other probe fails (a flaky route) on a managed link.
    let mut sc = Scenario::managed();
    sc.probe_fails = Box::new(|t| ((t * 1.7) as u64).is_multiple_of(2));
    let out = simulate(&sc);
    assert_eq!(out.time_in(Mode::Limited, 0.0, 600.0), 0.0);
    assert!(out.throughput(20.0, 600.0) >= 0.97 * 1.25 * MB);
}

#[test]
fn sim_very_slow_link_works() {
    // 256 kbit/s up with a 2 s buffer (old DSL, a weak mobile signal).
    let mut sc = Scenario::bloated();
    sc.cap = Box::new(|_| 32.0 * KB);
    sc.buffer_s = 2.0;
    sc.sockbuf = 64.0 * KB;
    let out = simulate(&sc);
    let base = simulate(&{
        let mut b = Scenario::bloated();
        b.cap = Box::new(|_| 32.0 * KB);
        b.buffer_s = 2.0;
        b.sockbuf = 64.0 * KB;
        b.controlled = false;
        b
    });
    let (a, b) = (out.throughput(120.0, 600.0), base.throughput(120.0, 600.0));
    assert!(a >= 0.8 * b, "{a} vs {b}\n{}", out.dump());
    assert!(
        out.delay_mean(120.0, 600.0) < 0.5 * base.delay_mean(120.0, 600.0),
        "{} vs {}\n{}",
        out.delay_mean(120.0, 600.0),
        base.delay_mean(120.0, 600.0),
        out.dump()
    );
}

/// Random links: the rules must hold for every one of them.
#[test]
fn sim_sweep_of_random_links() {
    let mut rng = Rng(0x5eed_1234_abcd);
    let mut report = Vec::new();
    for i in 0..120 {
        // Link speed 32 KB/s .. 12.5 MB/s (log-uniform).
        let cap = (32.0 * KB) * (400.0f64).powf(rng.unit());
        let base_ms = 5.0 + 595.0 * rng.unit().powi(3);
        // OS socket buffers grow to about a round trip of data: 0.1–2.5 s of the link.
        let sockbuf = (cap * (0.1 + 2.4 * rng.unit())).max(64.0 * KB);
        let jitter = 1.0 + 15.0 * rng.unit();
        let spikes = 0.04 * rng.unit();
        let seed = i + 1;
        let kind = i % 3;
        let mk = |controlled: bool| {
            let buffer_s = match kind {
                0 => 0.005 + 0.025 * (seed as f64 * 0.37).fract(), // managed queue
                1 => 0.3 + 1.7 * (seed as f64 * 0.61).fract(),     // bloated
                _ => 0.01,                                         // managed + outside delay
            };
            let extra = if kind == 2 {
                80.0 + 400.0 * (seed as f64 * 0.83).fract()
            } else {
                0.0
            };
            let extra_from = 30.0 + 200.0 * (seed as f64 * 0.29).fract();
            Scenario {
                cap: Box::new(move |_| cap),
                buffer_s,
                base_ms,
                sockbuf,
                demand: Box::new(|_| f64::INFINITY),
                other: Box::new(|_| 0.0),
                extra_ms: Box::new(move |t| if t > extra_from { extra } else { 0.0 }),
                noise: Box::new(move |r, _| {
                    r.exp(jitter) + if r.unit() < spikes { 300.0 } else { 0.0 }
                }),
                probe_fails: Box::new(|_| false),
                // Every other link: the service starts with uploads already running.
                start_s: if seed % 2 == 0 { 0.0 } else { 10.0 },
                enable_at: 0.0,
                secs: 600.0,
                seed,
                controlled,
            }
        };
        let out = simulate(&mk(true));
        let base = simulate(&mk(false));
        let (a, b) = (out.throughput(120.0, 600.0), base.throughput(120.0, 600.0));
        let limited = out.time_in(Mode::Limited, 0.0, 600.0);
        let desc = format!(
            "#{i} kind {kind}: link {:.0} KB/s, base {base_ms:.0} ms, sockbuf {:.0} KB: \
             throughput {:.1}% of uncontrolled, delay {:.0} vs {:.0} ms, limited {limited:.0} s, {} checks",
            cap / KB,
            sockbuf / KB,
            100.0 * a / b,
            out.delay_mean(120.0, 600.0),
            base.delay_mean(120.0, 600.0),
            out.checks
        );
        match kind {
            // Nothing to fix: no limit, no loss, and pauses stay rare.
            0 | 2 => {
                assert_eq!(limited, 0.0, "{desc}\n{}", out.dump());
                let paused = out.time_in(Mode::Checking, 0.0, 600.0);
                assert!(paused <= 30.0, "paused {paused} s: {desc}\n{}", out.dump());
                assert!(a >= 0.97 * b, "{desc}\n{}", out.dump());
            }
            // Bloated: most of the speed, much less delay.
            _ => {
                assert!(a >= 0.8 * b, "{desc}\n{}", out.dump());
                assert!(
                    out.delay_mean(120.0, 600.0) <= 0.5 * base.delay_mean(120.0, 600.0),
                    "{desc}\n{}",
                    out.dump()
                );
            }
        }
        report.push(desc);
    }
    eprintln!("{}", report.join("\n"));
}

#[test]
fn sim_pauses_stay_rare_over_two_hours() {
    // Started mid-upload, and from 2 min on the connection is 150 ms slower for a reason
    // that isn't ours (a VPN). Checks and calibrations back off: over two hours they cost
    // about 1 % of upload time, and nothing is ever limited.
    let mut sc = Scenario::managed();
    sc.start_s = 0.0;
    sc.secs = 7200.0;
    sc.extra_ms = Box::new(|t| if t > 120.0 { 150.0 } else { 0.0 });
    let out = simulate(&sc);
    assert_eq!(
        out.time_in(Mode::Limited, 0.0, 7200.0),
        0.0,
        "{}",
        out.dump()
    );
    let paused = out.time_in(Mode::Checking, 0.0, 7200.0);
    assert!(out.checks <= 9, "{} pauses\n{}", out.checks, out.dump());
    assert!(paused <= 75.0, "paused {paused} s\n{}", out.dump());
    eprintln!(
        "{} pauses, {paused:.1} s in two hours\n{}",
        out.checks,
        out.dump()
    );
}
