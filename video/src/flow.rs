// SPDX-License-Identifier: Apache-2.0 AND MIT
// Derived from voxl src/hub.rs (Client, DelayEst, RateEst) and src/video.rs
// (flow_ok, RateCtl) (MIT, Copyright (c) 2026 Tembo).
//! Per-viewer delivery tracking, flow control and adaptive bitrate.
//!
//! Every frame sent to a viewer is "in flight" until the viewer acknowledges
//! it. From the acks we learn the round-trip delay, how much of it is
//! queueing, and the rate the path actually delivers. Flow control holds
//! frames back while too many are in flight; the rate controller sets the
//! encoder's bitrate cap from the measured delivery.

use crate::log::Logger;
use std::collections::VecDeque;

/// Milliseconds from `since` to `now`; "long ago" when `since` is unset.
fn elapsed(since: Option<u64>, now: u64) -> u64 {
    since.map_or(u64::MAX, |s| now.saturating_sub(s))
}

fn wrapping_le(a: u32, b: u32) -> bool {
    b.wrapping_sub(a) < 0x8000_0000
}

pub struct Inflight {
    pub id: u32,
    pub sent_ms: u64,
    pub bytes: usize,
}

pub struct Viewer {
    pub id: u32,
    pub inflight: VecDeque<Inflight>,
    rtts: VecDeque<u32>,
    /// Smallest of the last 20 RTT samples (0: none yet).
    pub min_rtt_ms: u32,
    window_start_ms: u64,
    window_bytes: usize,
    /// Exponentially weighted delivered rate, from acks.
    pub rate_bps: f64,
    /// Send-to-ack delay: `base` is the slowly expiring minimum, the excess
    /// over it is queueing (network buffers or a slow decoder).
    base_ms: f64,
    base_at_ms: u64,
    pub queue_ms: f64,
    pub needs_keyframe: bool,
    /// Has received a keyframe since (re)starting.
    pub started: bool,
}

impl Viewer {
    pub fn new(id: u32, now_ms: u64) -> Self {
        Viewer {
            id,
            inflight: VecDeque::new(),
            rtts: VecDeque::new(),
            min_rtt_ms: 0,
            window_start_ms: now_ms,
            window_bytes: 0,
            rate_bps: 0.0,
            base_ms: f64::MAX,
            base_at_ms: now_ms,
            queue_ms: 0.0,
            needs_keyframe: true,
            started: false,
        }
    }

    pub fn rtt_or_default(&self) -> f64 {
        match self.min_rtt_ms {
            0 => 40.0,
            r => r as f64,
        }
    }

    /// Notes a round-trip time measured by the host (e.g. from guacd's sync
    /// instructions).
    pub fn on_rtt(&mut self, rtt_ms: u32) {
        self.rtts.push_back(rtt_ms);
        if self.rtts.len() > 20 {
            self.rtts.pop_front();
        }
        self.min_rtt_ms = *self.rtts.iter().min().unwrap();
    }

    /// Notes that the viewer has decoded every frame up to `frame_id`.
    pub fn on_ack(&mut self, frame_id: u32, now_ms: u64) {
        let mut acked = 0;
        let mut sample = None;
        while let Some(f) = self.inflight.front() {
            if !wrapping_le(f.id, frame_id) {
                break;
            }
            acked += f.bytes;
            if f.id == frame_id {
                sample = Some(now_ms.saturating_sub(f.sent_ms) as f64);
            }
            self.inflight.pop_front();
        }
        if let Some(ms) = sample {
            if ms < self.base_ms || now_ms.saturating_sub(self.base_at_ms) >= 10_000 {
                self.base_ms = ms;
                self.base_at_ms = now_ms;
            }
            let qd = ms - self.base_ms;
            self.queue_ms = self.queue_ms * 0.7 + qd * 0.3;
        }
        self.window_bytes += acked;
        let el = now_ms.saturating_sub(self.window_start_ms) as f64 / 1000.0;
        if el >= 0.5 {
            let r = self.window_bytes as f64 * 8.0 / el;
            self.rate_bps = if self.rate_bps == 0.0 {
                r
            } else {
                self.rate_bps * 0.6 + r * 0.4
            };
            self.window_bytes = 0;
            self.window_start_ms = now_ms;
        }
    }

    /// Frames in flight and the age of the oldest one.
    pub fn backlog(&self, now_ms: u64) -> (usize, u64) {
        (
            self.inflight.len(),
            self.inflight
                .front()
                .map_or(0, |f| now_ms.saturating_sub(f.sent_ms)),
        )
    }

    /// Starts over from a keyframe (stalled or hopelessly behind).
    pub fn resync(&mut self) {
        self.inflight.clear();
        self.needs_keyframe = true;
        self.started = false;
    }
}

/// Whether another frame may be sent now. Allows enough frames in flight to
/// cover one RTT plus decode, but never more than ~RTT + 120 ms of queue.
/// A viewer stalled for 4 s (hidden tab, dead socket) stops gating the others
/// and is resynced.
pub fn flow_ok(viewers: &mut [Viewer], interval_ms: f64, now_ms: u64, log: &Logger) -> bool {
    let mut ok = true;
    for v in viewers.iter_mut() {
        let (n, age) = v.backlog(now_ms);
        if n == 0 {
            continue;
        }
        let rtt = v.rtt_or_default();
        let max_n = (((rtt + 40.0) / interval_ms).ceil() as usize + 2).clamp(3, 40);
        let max_age = rtt as u64 + 120;
        if age > 4000 {
            log.warn(|| format!("viewer {} stalled ({n} frames, {age} ms); resyncing", v.id));
            v.resync();
            continue;
        }
        if n >= max_n || age > max_age {
            ok = false;
        }
    }
    ok
}

/// Starting bitrate cap for a new session (kbit/s).
pub const START_KBPS: f64 = 8000.0;

/// guac-extra tuning (voxl: 15 s, +2% per second). After congestion, the
/// delivery rate seen then is kept as the path's likely capacity for this
/// long; above 90% of it the cap grows by `PROBE_STEP` every
/// `PROBE_EVERY_MS`. voxl's values took 15-20 s to recover after a link got
/// faster; these take a few seconds, and back-off (15% per 300 ms) still
/// drains any queue the probe builds.
const CEILING_MEMORY_MS: u64 = 8_000;
const PROBE_EVERY_MS: u64 = 500;
const PROBE_STEP: f64 = 1.05;

/// Delay-based bitrate adaptation: back off toward the measured delivery
/// rate when acks show queueing, probe upward slowly when the path is clear.
pub struct RateCtl {
    pub target: f64,
    max: f64,
    min: f64,
    last_eval: Option<u64>,
    last_dec: Option<u64>,
    last_inc: Option<u64>,
    /// Ramp multiplicatively until the first congestion (like TCP slow start).
    slow_start: bool,
    /// Delivery rate seen at the last congestion: the path's likely capacity.
    ceiling: f64,
    ceiling_at: Option<u64>,
}

impl RateCtl {
    pub fn new(max_kbps: f64, min_kbps: f64) -> Self {
        RateCtl {
            target: START_KBPS.min(max_kbps),
            max: max_kbps,
            min: min_kbps.min(max_kbps),
            last_eval: None,
            last_dec: None,
            last_inc: None,
            slow_start: true,
            ceiling: f64::MAX,
            ceiling_at: None,
        }
    }

    /// Changes the floor (e.g. a quarter of it while the video is at half
    /// resolution, which needs about a quarter of the bits).
    pub fn set_min(&mut self, min_kbps: f64) {
        self.min = min_kbps.min(self.max);
        self.target = self.target.max(self.min);
    }

    /// Whether the target sits at the floor: the link can't take even the
    /// lowest bitrate the controller may choose.
    pub fn at_floor(&self) -> bool {
        self.target <= self.min * 1.05
    }

    /// When the controller last backed off because of congestion.
    pub fn last_congested(&self) -> Option<u64> {
        self.last_dec
    }

    pub fn update(&mut self, viewers: &[Viewer], now: u64, log: &Logger) {
        if elapsed(self.last_eval, now) < 100 {
            return;
        }
        self.last_eval = Some(now);
        let mut congested = false;
        let mut clear = true;
        let mut rate = f64::MAX;
        let mut delivered = 0f64;
        for v in viewers.iter().filter(|v| v.started) {
            let (_, age) = v.backlog(now);
            if v.queue_ms > 30.0 || age as f64 > v.rtt_or_default() + 80.0 {
                congested = true;
                let r = v.rate_bps / 1000.0;
                if r > 0.0 {
                    rate = rate.min(r);
                }
            }
            if v.queue_ms > 15.0 {
                clear = false;
            }
            delivered = delivered.max(v.rate_bps / 1000.0);
        }
        if congested && elapsed(self.last_dec, now) >= 300 {
            let mut t = self.target * 0.85;
            // Delivery only shows capacity when we pushed near the target; one
            // big frame on an idle screen would otherwise pin the ceiling low.
            if rate.is_finite() && rate >= self.target * 0.5 {
                self.ceiling = rate;
                self.ceiling_at = Some(now);
                t = t.min(rate * 0.85);
            }
            self.target = t.max(self.min);
            self.last_dec = Some(now);
            self.slow_start = false;
            log.debug(|| format!("rate: congested, target {:.0} kbps", self.target));
        } else if self.slow_start && !congested && clear && elapsed(self.last_inc, now) >= 250 {
            // Only probe upward once the content presses against the cap.
            if delivered >= self.target * 0.5 {
                self.target = (self.target * 1.5).min(self.max);
                self.last_inc = Some(now);
                log.debug(|| format!("rate: slow start, target {:.0} kbps", self.target));
            }
        } else if !congested
            && clear
            && elapsed(self.last_dec, now) >= 1000
            && delivered >= self.target * 0.5
        {
            // Forget a capacity estimate that hasn't been confirmed lately.
            if elapsed(self.ceiling_at, now) >= CEILING_MEMORY_MS {
                self.ceiling = f64::MAX;
            }
            // Recover quickly to just under the known capacity, then probe
            // past it gently.
            if self.target < self.ceiling * 0.9 && elapsed(self.last_inc, now) >= 200 {
                self.target = (self.target * 1.06 + 100.0)
                    .min(self.ceiling * 0.9)
                    .min(self.max);
                self.last_inc = Some(now);
            } else if self.target >= self.ceiling * 0.9
                && elapsed(self.last_inc, now) >= PROBE_EVERY_MS
            {
                self.target = (self.target * PROBE_STEP).min(self.max);
                self.last_inc = Some(now);
                log.debug(|| format!("rate: probing, target {:.0} kbps", self.target));
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn send(v: &mut Viewer, id: u32, now: u64, bytes: usize) {
        v.inflight.push_back(Inflight { id, sent_ms: now, bytes });
    }

    #[test]
    fn ack_clears_inflight_and_measures_rate() {
        let mut v = Viewer::new(1, 0);
        for i in 1..=10 {
            send(&mut v, i, i as u64 * 10, 10_000);
        }
        v.on_ack(10, 600);
        assert!(v.inflight.is_empty());
        // 100 kB over 0.6 s.
        assert!((v.rate_bps - 100_000.0 * 8.0 / 0.6).abs() < 1.0);
    }

    #[test]
    fn ack_ids_wrap() {
        let mut v = Viewer::new(1, 0);
        send(&mut v, u32::MAX, 0, 1);
        send(&mut v, 0, 1, 1);
        send(&mut v, 1, 2, 1);
        v.on_ack(0, 10);
        assert_eq!(v.inflight.len(), 1);
        assert_eq!(v.inflight[0].id, 1);
    }

    #[test]
    fn queueing_shows_as_queue_delay() {
        let mut v = Viewer::new(1, 0);
        send(&mut v, 1, 0, 1);
        v.on_ack(1, 20); // base 20 ms
        for i in 2..20 {
            send(&mut v, i, i as u64 * 100, 1);
            v.on_ack(i, i as u64 * 100 + 120); // 100 ms over base
        }
        assert!(v.queue_ms > 90.0, "{}", v.queue_ms);
    }

    #[test]
    fn flow_blocks_when_too_much_in_flight() {
        let log = Logger::default();
        let mut vs = vec![Viewer::new(1, 0)];
        vs[0].on_rtt(20);
        // 60 fps: max_n = ceil(60 / 16.7) + 2 = 6.
        for i in 0..5 {
            send(&mut vs[0], i, 0, 1);
        }
        assert!(flow_ok(&mut vs, 1000.0 / 60.0, 10, &log));
        send(&mut vs[0], 5, 0, 1);
        assert!(!flow_ok(&mut vs, 1000.0 / 60.0, 10, &log));
    }

    #[test]
    fn stalled_viewer_is_resynced_and_stops_blocking() {
        let log = Logger::default();
        let mut vs = vec![Viewer::new(1, 0)];
        vs[0].started = true;
        vs[0].needs_keyframe = false;
        for i in 0..50 {
            send(&mut vs[0], i, 0, 1);
        }
        assert!(flow_ok(&mut vs, 16.7, 5000, &log));
        assert!(vs[0].needs_keyframe && !vs[0].started && vs[0].inflight.is_empty());
    }

    #[test]
    fn congestion_lowers_target_toward_delivery() {
        let log = Logger::default();
        let mut rc = RateCtl::new(30_000.0, 2000.0);
        let mut v = Viewer::new(1, 0);
        v.started = true;
        v.queue_ms = 100.0;
        v.rate_bps = 5_000_000.0; // 5 Mbit/s delivered
        rc.update(std::slice::from_ref(&v), 1000, &log);
        assert!((rc.target - 4250.0).abs() < 1.0, "{}", rc.target);
    }

    /// After congestion on a 1 Mbit/s link, the link becomes fast. With the
    /// content pressing against the cap and no queue, the cap must at least
    /// double within 10 s.
    #[test]
    fn recovers_after_the_link_gets_faster() {
        let log = Logger::default();
        let mut rc = RateCtl::new(30_000.0, 500.0);
        let mut v = Viewer::new(1, 0);
        v.started = true;
        // Congested: queue builds, ~600 kbit/s delivered.
        v.queue_ms = 100.0;
        v.rate_bps = 600_000.0;
        let mut now = 0;
        for _ in 0..80 {
            now += 100;
            rc.update(std::slice::from_ref(&v), now, &log);
        }
        let low = rc.target;
        assert!(low <= 600.0, "{low}");
        // Fast link: no queue, and the content uses whatever the cap allows.
        v.queue_ms = 0.0;
        for _ in 0..100 {
            now += 100;
            v.rate_bps = rc.target * 1000.0 * 0.9;
            rc.update(std::slice::from_ref(&v), now, &log);
        }
        assert!(rc.target >= low * 2.0, "{low} -> {}", rc.target);
    }

    #[test]
    fn slow_start_ramps_when_content_uses_the_cap() {
        let log = Logger::default();
        let mut rc = RateCtl::new(30_000.0, 2000.0);
        let mut v = Viewer::new(1, 0);
        v.started = true;
        v.rate_bps = 7_000_000.0;
        rc.update(std::slice::from_ref(&v), 1000, &log);
        assert_eq!(rc.target, 12_000.0);
        // Idle content (delivers little) does not raise it.
        v.rate_bps = 100_000.0;
        rc.update(std::slice::from_ref(&v), 2000, &log);
        assert_eq!(rc.target, 12_000.0);
    }
}
