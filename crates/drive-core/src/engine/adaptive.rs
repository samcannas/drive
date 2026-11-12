//! Automatic upload speed: uploads slow down when they make the rest of the connection
//! sluggish, and only then.
//!
//! # The problem
//!
//! A home connection's upstream link usually has a large buffer in the modem or router. A
//! bulk upload fills it, and every other packet leaving the house (a video call, a web page
//! request, a game) waits behind seconds of our data: the internet "feels slow" although the
//! link is simply full. That waiting time is *queuing delay*.
//!
//! # Measuring
//!
//! While uploading, the service times TCP handshakes to the uloz.to API host (a SYN out, a
//! SYN-ACK back; nothing else is sent). The handshake's SYN waits in the same upstream queue
//! as our data, so `handshake time − the fastest handshake recently seen (the base)` is the
//! queuing delay. This is the idea of LEDBAT (RFC 6817), the "background traffic" congestion
//! control used by BitTorrent's uTP and OS updates, applied as a rate limit.
//!
//! # Never slowing down without cause
//!
//! High latency alone is not evidence that *we* are the problem: the delay may come from
//! someone else's traffic, a full download link, Wi-Fi interference or a VPN. So:
//!
//! 1. Nothing happens unless the (spike-filtered) queuing delay stays above
//!    [`Params::engage_ms`] for [`Params::engage_for_ms`] while we upload at a meaningful rate.
//! 2. Then a **check**: uploads pause for up to [`Params::check_max_ms`]. If our data was
//!    what filled the queue, it drains and the delay falls back near the base within seconds;
//!    only then is a limit set (somewhat below the rate we were sending at). If the delay
//!    stays up, it isn't ours: uploads continue at full speed and no check runs again for
//!    [`Params::cooldown_ms`].
//! 3. While limited, the limit follows the measured delay towards [`Params::target_ms`]
//!    (slower when the queue grows, faster when it empties), and the limit is dropped
//!    entirely as soon as it stops mattering: uploads end, the limit stops being what holds
//!    uploads back, it climbs past the fastest rate seen, or measurements stop arriving
//!    (fail open). Stuck at the floor with the delay still high means something else is
//!    going on: a new check decides.
//!
//! The base is only right if some handshake saw an empty queue. When uploads were already
//! running before the first measurement (the service started mid-upload: a resumed chunked
//! upload keeps the queue full for hours), every sample is inflated and so is the base,
//! which hides the queue. So when no sample was taken while our uploads weren't running,
//! uploads pause once for a **calibration** (at most [`Params::check_abort_ms`]): a round
//! trip that drops by more than [`Params::engage_ms`] reveals our queue and limiting starts
//! as after a check; otherwise the base was right and is trusted for
//! [`Params::calibrated_for_ms`].
//!
//! The controller here is pure (time and samples are inputs) so it can be tested against a
//! simulated network; [`run`] connects it to a real probe and the upload [`Throttle`].

use std::collections::VecDeque;
use std::net::SocketAddr;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use tokio::time::Instant;
use tokio_util::sync::CancellationToken;
use tracing::{debug, info};

use super::throttle::Throttle;

/// Tuning. The defaults are what the service uses; tests vary them only to check behaviour.
#[derive(Debug, Clone)]
pub struct Params {
    /// Queuing delay the limit aims for while active.
    pub target_ms: f64,
    /// Queuing delay that starts a check...
    pub engage_ms: f64,
    /// ...once it has lasted this long.
    pub engage_for_ms: u64,
    /// Our own sending rate (bytes/s) below which we can't be filling anyone's link.
    pub min_upload_rate: f64,
    /// A check gives the queue at least this long to drain before judging...
    pub check_min_ms: u64,
    /// ...and at most this long.
    pub check_max_ms: u64,
    /// A check passes when the delay falls to `max(clear_ms, clear_frac × delay before)`.
    pub clear_ms: f64,
    pub clear_frac: f64,
    /// A check that sees the delay not move at all (within 10 %) after this long ends early:
    /// pausing clearly makes no difference.
    pub check_abort_ms: u64,
    /// No new check this long after one found that the delay wasn't ours; doubled after
    /// each further such check in a row, up to `max_cooldown_ms`.
    pub cooldown_ms: u64,
    pub max_cooldown_ms: u64,
    /// The limit never goes below `max(floor_rate, floor_frac × fastest rate seen)`.
    pub floor_rate: f64,
    pub floor_frac: f64,
    /// A newly set limit starts at this fraction of the rate before the check.
    pub start_frac: f64,
    /// Below the target, the limit grows by `gain × (target − delay) / target` per second,
    /// at most `max_up` per second, and never past `headroom ×` the rate actually achieved.
    pub gain: f64,
    pub max_up: f64,
    /// Near the rate at which the queue last built up (the knee), growth slows to
    /// `max_up_near_knee` per second: probe gently where the link's limit was, as TCP Cubic
    /// does. Past `1.1 ×` the knee without a queue, the link got faster and it's forgotten.
    pub max_up_near_knee: f64,
    pub headroom: f64,
    /// Above the target the link is full, so the rate that filled it is its speed: the limit
    /// goes to that rate minus enough to drain the excess queue over this long.
    pub drain_horizon_ms: f64,
    /// That rate is an estimate a few percent off; while the queue stays above the target the
    /// estimate shrinks by `integral × min(1, excess / target)` per second until it drains.
    pub integral: f64,
    /// Release when the limit has been at least `unbound_margin ×` what uploads achieve,
    /// with a short queue, for this long: it isn't what holds them back.
    pub unbound_ms: u64,
    pub unbound_margin: f64,
    /// The delay stays above `engage_ms` this long despite the limit: check again whether
    /// it is ours at all.
    pub stuck_ms: u64,
    /// No bytes uploaded this long: release.
    pub idle_ms: u64,
    /// No measurement this long: release (fail open).
    pub sample_timeout_ms: u64,
    /// Spike filter: the delay is the lowest sample of this window (or the latest sample).
    pub filter_ms: u64,
    /// The base is the lowest sample of the last `base_minutes` one-minute buckets.
    pub base_minutes: u64,
    /// Samples needed before the base is trusted.
    pub base_min_samples: usize,
    /// A calibration that found no queue makes the base trusted for this long.
    pub calibrated_for_ms: u64,
    /// Smoothing time constant of the measured upload rate.
    pub rate_tau_ms: f64,
    /// Probe intervals. The first `warmup_samples` come quickly so the base is known before
    /// uploads (which usually start right after launch) fill the queue.
    pub warmup_samples: usize,
    pub interval_warmup_ms: u64,
    pub interval_idle_ms: u64,
    pub interval_active_ms: u64,
    pub interval_limited_ms: u64,
    pub interval_check_ms: u64,
}

impl Default for Params {
    fn default() -> Self {
        Self {
            target_ms: 60.0,
            engage_ms: 120.0,
            engage_for_ms: 6_000,
            min_upload_rate: 16.0 * 1024.0,
            check_min_ms: 500,
            check_max_ms: 8_000,
            clear_ms: 40.0,
            clear_frac: 0.25,
            check_abort_ms: 6_000,
            cooldown_ms: 5 * 60_000,
            max_cooldown_ms: 30 * 60_000,
            floor_rate: 16.0 * 1024.0,
            floor_frac: 0.05,
            start_frac: 0.8,
            gain: 0.25,
            max_up: 0.08,
            max_up_near_knee: 0.01,
            headroom: 1.2,
            drain_horizon_ms: 3_000.0,
            integral: 0.04,
            unbound_ms: 20_000,
            unbound_margin: 1.15,
            stuck_ms: 20_000,
            idle_ms: 30_000,
            sample_timeout_ms: 6_000,
            filter_ms: 3_200,
            base_minutes: 10,
            base_min_samples: 3,
            calibrated_for_ms: 30 * 60_000,
            rate_tau_ms: 3_000.0,
            warmup_samples: 5,
            interval_warmup_ms: 200,
            interval_idle_ms: 15_000,
            interval_active_ms: 1_000,
            interval_limited_ms: 500,
            interval_check_ms: 250,
        }
    }
}

/// What the throttle should do now.
#[derive(Debug, Clone, Copy, PartialEq, Default)]
pub struct Decision {
    /// Automatic limit in bytes/s (`None` = none).
    pub limit: Option<u64>,
    /// Pause uploads (a check is running).
    pub hold: bool,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, serde::Serialize)]
#[serde(rename_all = "snake_case")]
pub enum Mode {
    #[default]
    Unlimited,
    Checking,
    Limited,
}

/// For status displays and logs.
#[derive(Debug, Clone, Copy, PartialEq, Default)]
pub struct Snapshot {
    pub mode: Mode,
    /// Bytes/s while [`Mode::Limited`].
    pub limit: Option<u64>,
    pub base_ms: Option<f64>,
    pub delay_ms: Option<f64>,
    /// Smoothed upload rate, bytes/s.
    pub rate: f64,
}

#[derive(Debug, Clone, Copy)]
enum State {
    Unlimited {
        high_since: Option<u64>,
        /// Uploading with a base no idle sample has confirmed, since.
        unverified_since: Option<u64>,
    },
    Checking {
        since: u64,
        rate_before: f64,
        delay_before: f64,
        /// A calibration: the (filtered) round trip before pausing, judged against the base
        /// learned during the pause.
        calibrating: Option<f64>,
    },
    Limited {
        rate: f64,
        /// While the queue stands above the target: the rate that filled the link.
        anchor: Option<f64>,
        unbound_since: Option<u64>,
        stuck_since: Option<u64>,
    },
}

const FREE: State = State::Unlimited {
    high_since: None,
    unverified_since: None,
};

pub struct Controller {
    p: Params,
    state: State,
    /// Recent samples: (time ms, round trip ms).
    samples: VecDeque<(u64, f64)>,
    /// One-minute buckets: (minute, lowest round trip ms, count).
    base: VecDeque<(u64, f64, usize)>,
    last_sample_at: Option<u64>,
    last_tick: Option<(u64, u64)>,
    rate: f64,
    peak: f64,
    last_active: Option<u64>,
    /// Whether an upload was in progress at the last tick.
    uploads_active: bool,
    /// While limited: the rate at which the queue last built up.
    knee: Option<f64>,
    cooldown_until: u64,
    /// Last sample taken while our uploads weren't running (it could see an empty queue).
    last_idle_sample: Option<u64>,
    /// A calibration found no queue: the base is trusted until then.
    trusted_until: u64,
    /// Checks in a row that found the delay wasn't ours.
    negatives: u32,
    /// Transitions since the last [`Controller::take_events`], for logging.
    events: Vec<String>,
}

impl Controller {
    pub fn new(p: Params) -> Self {
        Self {
            p,
            state: FREE,
            samples: VecDeque::new(),
            base: VecDeque::new(),
            last_sample_at: None,
            last_tick: None,
            rate: 0.0,
            peak: 0.0,
            last_active: None,
            uploads_active: false,
            knee: None,
            cooldown_until: 0,
            last_idle_sample: None,
            trusted_until: 0,
            negatives: 0,
            events: Vec::new(),
        }
    }

    pub fn params(&self) -> &Params {
        &self.p
    }

    /// A measured handshake time.
    pub fn on_sample(&mut self, now: u64, rtt_ms: f64) {
        if !rtt_ms.is_finite() || rtt_ms < 0.0 {
            return;
        }
        self.last_sample_at = Some(now);
        self.samples.push_back((now, rtt_ms));
        while self.samples.len() > 64 {
            self.samples.pop_front();
        }
        let minute = now / 60_000;
        match self.base.back_mut() {
            Some((m, lo, n)) if *m == minute => {
                *lo = lo.min(rtt_ms);
                *n += 1;
            }
            _ => self.base.push_back((minute, rtt_ms, 1)),
        }
        let keep_from = minute.saturating_sub(self.p.base_minutes - 1);
        while self.base.front().is_some_and(|(m, _, _)| *m < keep_from) {
            self.base.pop_front();
        }
    }

    /// The lowest recent round trip: the path without any queue.
    pub fn base_ms(&self) -> Option<f64> {
        let n: usize = self.base.iter().map(|b| b.2).sum();
        if n < self.p.base_min_samples {
            return None;
        }
        self.base.iter().map(|b| b.1).reduce(f64::min)
    }

    /// Queuing delay for regulating a limit: the median of the last three samples. It reacts
    /// within two samples (the minimum filter needs a whole window, which let the queue grow
    /// far past the target) and still ignores a single spike.
    fn delay_median_ms(&self, now: u64) -> Option<f64> {
        let base = self.base_ms()?;
        let (last_t, _) = *self.samples.back()?;
        if now.saturating_sub(last_t) > self.p.sample_timeout_ms {
            return None;
        }
        let mut v: Vec<f64> = self.samples.iter().rev().take(3).map(|s| s.1).collect();
        v.sort_by(f64::total_cmp);
        Some((v[v.len() / 2] - base).max(0.0))
    }

    /// Queuing delay with spikes filtered out: the lowest sample of the filter window.
    fn delay_ms(&self, now: u64) -> Option<f64> {
        let base = self.base_ms()?;
        let (last_t, last) = *self.samples.back()?;
        if now.saturating_sub(last_t) > self.p.sample_timeout_ms {
            return None;
        }
        let lo = self
            .samples
            .iter()
            .rev()
            .take_while(|(t, _)| last_t.saturating_sub(*t) <= self.p.filter_ms)
            .map(|s| s.1)
            .fold(last, f64::min);
        Some((lo - base).max(0.0))
    }

    /// Latest unfiltered queuing delay after `after` (checks look for any low sample: noise
    /// only ever adds delay, so one low sample proves the queue drained).
    fn lowest_since(&self, after: u64) -> Option<f64> {
        let base = self.base_ms()?;
        self.samples
            .iter()
            .filter(|(t, _)| *t >= after)
            .map(|s| (s.1 - base).max(0.0))
            .reduce(f64::min)
    }

    /// Whether some recent sample could have seen an empty queue.
    fn base_verified(&self, now: u64) -> bool {
        now < self.trusted_until
            || self
                .last_idle_sample
                .is_some_and(|t| now.saturating_sub(t) <= self.p.base_minutes * 60_000)
    }

    /// Lowest round trip of the filter window (no base subtracted).
    fn filtered_rtt(&self) -> Option<f64> {
        let (last_t, last) = *self.samples.back()?;
        Some(
            self.samples
                .iter()
                .rev()
                .take_while(|(t, _)| last_t.saturating_sub(*t) <= self.p.filter_ms)
                .map(|s| s.1)
                .fold(last, f64::min),
        )
    }

    fn fresh(&self, now: u64) -> bool {
        self.last_sample_at
            .is_some_and(|t| now.saturating_sub(t) <= self.p.sample_timeout_ms)
    }

    /// After a check found the delay wasn't ours: wait before the next, longer each time.
    fn cool_down(&mut self, now: u64) -> u64 {
        let ms = self
            .p
            .cooldown_ms
            .saturating_mul(1u64 << self.negatives.min(16))
            .min(self.p.max_cooldown_ms);
        self.negatives = self.negatives.saturating_add(1);
        self.cooldown_until = now + ms;
        // The pause was the best measurement there is: another calibration can't show more
        // (without this, an unverified base re-triggered a calibration every few seconds).
        self.trusted_until = self.trusted_until.max(now + self.p.calibrated_for_ms);
        ms
    }

    fn floor(&self) -> f64 {
        self.p.floor_rate.max(self.p.floor_frac * self.peak)
    }

    /// Advance to `now`. `passed_total` is the throttle's running count of uploaded bytes;
    /// `uploads_active` whether an upload is in progress (the engine knows; byte flow alone
    /// can't tell: OS socket buffers take data in bursts seconds apart).
    pub fn tick(&mut self, now: u64, passed_total: u64, uploads_active: bool) -> Decision {
        let (dt_ms, bytes) = match self.last_tick {
            Some((t, b)) => (now.saturating_sub(t), passed_total.saturating_sub(b)),
            None => (0, 0),
        };
        // A sample taken since the previous tick saw no uploads of ours if none was in
        // progress over the whole interval (before the first interval nothing is known).
        if let Some((prev, _)) = self.last_tick
            && !uploads_active
            && !self.uploads_active
            && bytes == 0
            && dt_ms > 0
            && self.last_sample_at.is_some_and(|t| t >= prev)
        {
            self.last_idle_sample = self.last_sample_at;
        }
        self.uploads_active = uploads_active;
        self.last_tick = Some((now, passed_total));
        if bytes > 0 || uploads_active {
            self.last_active = Some(now);
        }
        let holding = matches!(self.state, State::Checking { .. });
        if dt_ms > 0 && !holding {
            let inst = bytes as f64 * 1000.0 / dt_ms as f64;
            let a = 1.0 - (-(dt_ms as f64) / self.p.rate_tau_ms).exp();
            self.rate += a * (inst - self.rate);
            if matches!(self.state, State::Unlimited { .. }) {
                // The fastest recent rate, forgetting slowly (10 min) when the link changes.
                let forget = (dt_ms as f64 / 600_000.0).min(1.0);
                self.peak = self
                    .rate
                    .max(self.peak - (self.peak - self.rate).max(0.0) * forget);
            }
        }
        let dt = dt_ms as f64 / 1000.0;
        let active = self
            .last_active
            .is_some_and(|t| now.saturating_sub(t) <= self.p.idle_ms);
        let uploading = uploads_active && self.rate >= self.p.min_upload_rate;
        let delay = self.delay_ms(now);
        let fresh = self.fresh(now);

        self.state = match self.state {
            State::Unlimited {
                high_since,
                unverified_since,
            } => {
                let unverified = uploading && fresh && delay.is_some() && !self.base_verified(now);
                let unverified_since = if unverified {
                    unverified_since.or(Some(now))
                } else {
                    None
                };
                let high = uploading
                    && now >= self.cooldown_until
                    && delay.is_some_and(|d| d > self.p.engage_ms);
                let high_since = if high { high_since.or(Some(now)) } else { None };
                if unverified_since.is_some_and(|s| now.saturating_sub(s) >= self.p.engage_for_ms) {
                    let rtt = self.filtered_rtt().unwrap_or_default();
                    self.event(format!(
                        "uploads were already running when measuring started (round trip \
                         {rtt:.0} ms): pausing them briefly to measure the connection without them"
                    ));
                    State::Checking {
                        since: now,
                        rate_before: self.rate,
                        delay_before: 0.0,
                        calibrating: Some(rtt),
                    }
                } else if high_since.is_some_and(|s| now.saturating_sub(s) >= self.p.engage_for_ms)
                {
                    let d = delay.unwrap_or_default();
                    self.event(format!(
                        "connection delay {d:.0} ms over the base while uploading at {}: \
                         pausing uploads briefly to see whether they cause it",
                        rate_text(self.rate)
                    ));
                    State::Checking {
                        since: now,
                        rate_before: self.rate,
                        delay_before: d,
                        calibrating: None,
                    }
                } else {
                    State::Unlimited {
                        high_since,
                        unverified_since,
                    }
                }
            }
            State::Checking {
                since,
                rate_before,
                delay_before,
                calibrating,
            } => {
                let elapsed = now.saturating_sub(since);
                // A calibration judges the round trip before the pause against the base
                // learned during it, and only a drop beyond the engage threshold counts as
                // our queue (a small drop is ordinary noise).
                let delay_before = match (calibrating, self.base_ms()) {
                    (Some(rtt), Some(base)) => (rtt - base).max(0.0),
                    _ => delay_before,
                };
                let queue_seen = calibrating.is_none() || delay_before > self.p.engage_ms;
                let clear = self.p.clear_ms.max(self.p.clear_frac * delay_before);
                let low = self.lowest_since(since + self.p.check_min_ms.min(elapsed));
                if !fresh {
                    self.event(
                        "no connection measurements during the check: uploading at full speed"
                            .into(),
                    );
                    self.cool_down(now);
                    FREE
                } else if queue_seen
                    && (elapsed >= self.p.check_min_ms && low.is_some_and(|l| l <= clear)
                        || elapsed >= self.p.check_max_ms
                            && low.is_some_and(|l| l <= 0.5 * delay_before))
                {
                    // Drained (or, with deep OS socket buffers still emptying, at least halved:
                    // mostly ours). Someone else's traffic would refill what we vacate instead.
                    let rate = (rate_before * self.p.start_frac).max(self.floor());
                    self.negatives = 0;
                    self.knee = None;
                    self.trusted_until = self.trusted_until.max(now + self.p.calibrated_for_ms);
                    self.event(format!(
                        "the delay fell from {delay_before:.0} ms to {:.0} ms with uploads paused: \
                         uploads cause it, limiting them to {}",
                        low.unwrap_or_default(),
                        rate_text(rate)
                    ));
                    State::Limited {
                        rate,
                        anchor: None,
                        unbound_since: None,
                        stuck_since: None,
                    }
                } else if calibrating.is_some() && !queue_seen && elapsed >= self.p.check_abort_ms {
                    self.trusted_until = now + self.p.calibrated_for_ms;
                    self.event(format!(
                        "measured the connection with uploads paused: base {:.0} ms, no queue to fix",
                        self.base_ms().unwrap_or_default()
                    ));
                    FREE
                } else if elapsed >= self.p.check_max_ms
                    || elapsed >= self.p.check_abort_ms
                        && low.is_some_and(|l| l >= 0.9 * delay_before)
                {
                    let ms = self.cool_down(now);
                    self.event(format!(
                        "the delay stayed at {:.0} ms with uploads paused: something else causes it, \
                         uploading at full speed (no new check for {} min)",
                        low.unwrap_or(delay_before),
                        ms / 60_000
                    ));
                    FREE
                } else {
                    self.state
                }
            }
            State::Limited {
                mut rate,
                anchor,
                unbound_since,
                stuck_since,
            } => {
                let d = self.delay_median_ms(now);
                if !active {
                    self.event("uploads finished: limit removed".into());
                    FREE
                } else if !fresh || d.is_none() {
                    self.event("no connection measurements: limit removed".into());
                    FREE
                } else {
                    let d = d.unwrap_or_default();
                    let floor = self.floor();
                    let target = self.p.target_ms;
                    let anchor = if d > target {
                        // A standing queue: the link is full. While the limit isn't what
                        // holds uploads back, the measured rate is the link's speed.
                        let mut a = anchor.unwrap_or(self.rate);
                        if self.rate < 0.95 * rate {
                            a = a.min(self.rate);
                        }
                        let excess = ((d - target) / target).min(1.0);
                        a *= 1.0 - self.p.integral * excess * dt;
                        let drain = ((d - target) / self.p.drain_horizon_ms).min(0.5);
                        rate = (a * (1.0 - drain)).max(floor);
                        Some(a)
                    } else {
                        // The queue just drained: the rate that had filled it is the knee.
                        if let Some(a) = anchor {
                            self.knee = Some(a);
                        }
                        if self.knee.is_some_and(|k| rate > 1.1 * k) {
                            self.knee = None; // the link got faster
                        }
                        // Short queue: grow, slowly near the knee, and not far past what
                        // uploads actually achieve (an unused limit could otherwise run far
                        // above the link's speed).
                        let max_up = if self.knee.is_some_and(|k| rate >= 0.95 * k) {
                            self.p.max_up_near_knee
                        } else {
                            self.p.max_up
                        };
                        let up = (self.p.gain * (target - d) / target).min(max_up) * dt;
                        let grown = rate * (1.0 + up);
                        let ceiling = (self.rate * self.p.headroom).max(rate);
                        rate = grown.min(ceiling).max(floor);
                        None
                    };
                    // Not binding: the limit sits well above what uploads achieve (small
                    // files, or the limit drifted above the link's speed) and the queue is
                    // fine. A link that got faster needs no release: the limit follows it up.
                    let unbound = self.rate * self.p.unbound_margin < rate && d <= target;
                    let unbound_since = if unbound {
                        unbound_since.or(Some(now))
                    } else {
                        None
                    };
                    let stuck = d > self.p.engage_ms;
                    let stuck_since = if stuck {
                        stuck_since.or(Some(now))
                    } else {
                        None
                    };
                    if unbound_since.is_some_and(|s| now.saturating_sub(s) >= self.p.unbound_ms) {
                        self.event(format!(
                            "uploads use {} of the {} limit with a short queue: limit removed",
                            rate_text(self.rate),
                            rate_text(rate)
                        ));
                        FREE
                    } else if stuck_since.is_some_and(|s| now.saturating_sub(s) >= self.p.stuck_ms)
                    {
                        self.event(format!(
                            "the delay stays at {d:.0} ms despite the {} limit: checking again",
                            rate_text(rate)
                        ));
                        State::Checking {
                            since: now,
                            rate_before: self.rate,
                            delay_before: d,
                            calibrating: None,
                        }
                    } else {
                        State::Limited {
                            rate,
                            anchor,
                            unbound_since,
                            stuck_since,
                        }
                    }
                }
            }
        };
        self.decision()
    }

    fn event(&mut self, e: String) {
        self.events.push(e);
    }

    /// Transitions since the last call (for the log).
    pub fn take_events(&mut self) -> Vec<String> {
        std::mem::take(&mut self.events)
    }

    pub fn decision(&self) -> Decision {
        match self.state {
            State::Unlimited { .. } => Decision::default(),
            State::Checking { .. } => Decision {
                limit: None,
                hold: true,
            },
            State::Limited { rate, .. } => Decision {
                limit: Some(rate.round().max(1.0) as u64),
                hold: false,
            },
        }
    }

    pub fn mode(&self) -> Mode {
        match self.state {
            State::Unlimited { .. } => Mode::Unlimited,
            State::Checking { .. } => Mode::Checking,
            State::Limited { .. } => Mode::Limited,
        }
    }

    pub fn snapshot(&self, now: u64) -> Snapshot {
        Snapshot {
            mode: self.mode(),
            limit: self.decision().limit,
            base_ms: self.base_ms(),
            delay_ms: self.delay_ms(now),
            rate: self.rate,
        }
    }

    /// How long to wait before the next measurement.
    pub fn interval(&self, _now: u64) -> Duration {
        let uploading = self.uploads_active;
        let warming = self.base.iter().map(|b| b.2).sum::<usize>() < self.p.warmup_samples;
        Duration::from_millis(match self.state {
            State::Checking { .. } => self.p.interval_check_ms,
            _ if warming => self.p.interval_warmup_ms,
            State::Limited { .. } => self.p.interval_limited_ms,
            State::Unlimited { .. } if uploading => self.p.interval_active_ms,
            State::Unlimited { .. } => self.p.interval_idle_ms,
        })
    }

    /// Back to unlimited (the feature was switched off).
    pub fn reset(&mut self) {
        if !matches!(self.state, State::Unlimited { .. }) {
            self.event("automatic upload speed turned off".into());
        }
        self.state = FREE;
    }
}

pub fn rate_text(bytes_per_sec: f64) -> String {
    if bytes_per_sec >= 1024.0 * 1024.0 {
        format!("{:.1} MB/s", bytes_per_sec / (1024.0 * 1024.0))
    } else {
        format!("{:.0} KB/s", bytes_per_sec / 1024.0)
    }
}

// ----- Measuring

/// Times one connection handshake.
#[async_trait::async_trait]
pub trait Probe: Send {
    /// `Some(round trip)`, or `None` when the probe failed (no answer at all).
    async fn rtt(&mut self) -> Option<Duration>;
}

/// TCP handshake time to `host:port`. Only a SYN and the closing packets are sent.
pub struct TcpProbe {
    target: String,
    addr: Option<(SocketAddr, Instant)>,
    timeout: Duration,
}

impl TcpProbe {
    /// `target` is `host:port`.
    pub fn new(target: impl Into<String>) -> Self {
        Self {
            target: target.into(),
            addr: None,
            timeout: Duration::from_secs(3),
        }
    }

    async fn resolve(&mut self) -> Option<SocketAddr> {
        if let Some((a, at)) = self.addr
            && at.elapsed() < Duration::from_secs(300)
        {
            return Some(a);
        }
        let a = tokio::time::timeout(self.timeout, tokio::net::lookup_host(self.target.as_str()))
            .await
            .ok()?
            .ok()?
            .next()?;
        self.addr = Some((a, Instant::now()));
        Some(a)
    }
}

#[async_trait::async_trait]
impl Probe for TcpProbe {
    async fn rtt(&mut self) -> Option<Duration> {
        let addr = self.resolve().await?;
        let start = Instant::now();
        match tokio::time::timeout(self.timeout, tokio::net::TcpStream::connect(addr)).await {
            Ok(Ok(stream)) => {
                let rtt = start.elapsed();
                drop(stream);
                Some(rtt)
            }
            // No answer within the timeout: as slow as it gets (a deep queue can do that).
            Err(_) => Some(self.timeout),
            Ok(Err(e)) => {
                debug!("connection probe to {} failed: {e}", self.target);
                self.addr = None;
                None
            }
        }
    }
}

/// Runs the controller against `probe` and applies its decisions to `throttle` until
/// `cancel`. `enabled` is read before every measurement, `uploads_active` (whether an upload
/// is in progress) at every tick; `shared` receives the latest state.
pub async fn run(
    throttle: Arc<Throttle>,
    mut probe: impl Probe,
    enabled: impl Fn() -> bool + Send,
    uploads_active: impl Fn() -> bool + Send,
    shared: Arc<Mutex<Snapshot>>,
    params: Params,
    cancel: CancellationToken,
) {
    let mut ctl = Controller::new(params);
    let epoch = Instant::now();
    let now_ms = || epoch.elapsed().as_millis() as u64;
    loop {
        let started = Instant::now();
        let wait = if enabled() {
            if let Some(rtt) = probe.rtt().await {
                ctl.on_sample(now_ms(), rtt.as_secs_f64() * 1000.0);
            }
            let now = now_ms();
            let d = ctl.tick(now, throttle.passed(), uploads_active());
            // The decision lapses by itself if this loop ever stops refreshing it.
            throttle.set_auto(d.limit, d.hold, Duration::from_secs(10));
            for e in ctl.take_events() {
                info!("automatic upload speed: {e}");
            }
            *shared.lock().unwrap() = ctl.snapshot(now);
            ctl.interval(now)
        } else {
            ctl.reset();
            for e in ctl.take_events() {
                info!("automatic upload speed: {e}");
            }
            throttle.set_auto(None, false, Duration::ZERO);
            *shared.lock().unwrap() = Snapshot::default();
            Duration::from_secs(5)
        };
        tokio::select! {
            _ = cancel.cancelled() => break,
            _ = tokio::time::sleep(wait.saturating_sub(started.elapsed())) => {}
        }
    }
    throttle.set_auto(None, false, Duration::ZERO);
}

#[cfg(test)]
#[path = "adaptive_tests.rs"]
mod tests;
