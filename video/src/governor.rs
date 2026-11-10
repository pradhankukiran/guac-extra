// SPDX-License-Identifier: Apache-2.0 AND MIT
// Derived from voxl src/video.rs ThreadGovernor and CostFit
// (MIT, Copyright (c) 2026 Tembo).
//! Decides when a stripe's encoder should use more threads.
//!
//! x264's sliced threading costs ~3x the CPU of one thread on desktop
//! content, so a stripe encodes single-threaded and only fans out while
//! sustained heavy motion would otherwise miss the frame budget.

/// Exponentially weighted least squares fit of encode ms = a + b * changed MBs.
#[derive(Default)]
pub struct CostFit {
    w: f64,
    x: f64,
    y: f64,
    xx: f64,
    xy: f64,
}

impl CostFit {
    pub fn add(&mut self, x: f64, y: f64) {
        let k = 0.995;
        self.w = self.w * k + 1.0;
        self.x = self.x * k + x;
        self.y = self.y * k + y;
        self.xx = self.xx * k + x * x;
        self.xy = self.xy * k + x * y;
    }

    pub fn predict(&self, x: f64) -> Option<f64> {
        if self.w < 30.0 {
            return None;
        }
        let var = self.xx * self.w - self.x * self.x;
        if var <= f64::EPSILON * self.w * self.w {
            return Some(self.y / self.w);
        }
        let b = (self.xy * self.w - self.x * self.y) / var;
        let a = (self.y - b * self.x) / self.w;
        Some(a + b * x)
    }
}

pub struct ThreadGovernor {
    pub threads: u32,
    pub ewma_ms: f64,
    heavy: u32,
    light_since_ms: Option<u64>,
    budget_ms: f64,
    /// One-thread encode time vs changed macroblocks, learned while on one thread.
    fit: CostFit,
    /// Predicted one-thread time while threaded.
    predicted_ms: f64,
}

impl ThreadGovernor {
    pub fn new(fps: u32) -> Self {
        ThreadGovernor {
            threads: 1,
            ewma_ms: 0.0,
            heavy: 0,
            light_since_ms: None,
            budget_ms: 1000.0 / fps.max(1) as f64,
            fit: CostFit::default(),
            predicted_ms: 0.0,
        }
    }

    /// Notes an encode; returns a new thread count when it should change.
    pub fn want(&mut self, enc_ms: f64, changed_mbs: f64, max: u32, now_ms: u64) -> Option<u32> {
        self.ewma_ms = self.ewma_ms * 0.8 + enc_ms * 0.2;
        if self.threads == 1 {
            self.fit.add(changed_mbs, enc_ms);
            // Switch before one thread runs out of budget: at ~90% of it on
            // average, enough frames miss it to drop the frame rate.
            self.heavy = if self.ewma_ms > self.budget_ms * 0.65 {
                self.heavy + 1
            } else {
                0
            };
            if self.heavy >= 20 {
                self.predicted_ms = self.ewma_ms;
                return Some(max);
            }
            return None;
        }
        let one = self.fit.predict(changed_mbs).unwrap_or(enc_ms);
        self.predicted_ms = self.predicted_ms * 0.8 + one * 0.2;
        if self.predicted_ms < self.budget_ms * 0.5 {
            // Switching reopens the encoder (a keyframe), so not on a brief lull.
            let since = *self.light_since_ms.get_or_insert(now_ms);
            (now_ms.saturating_sub(since) >= 2000).then_some(1)
        } else {
            self.light_since_ms = None;
            None
        }
    }

    /// Records that the encoder now runs `threads` threads.
    pub fn switched(&mut self, threads: u32) {
        self.threads = threads;
        self.heavy = 0;
        self.light_since_ms = None;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn fit_learns_a_line() {
        let mut f = CostFit::default();
        for i in 0..100 {
            let x = (i % 10) as f64 * 100.0;
            f.add(x, 2.0 + 0.01 * x);
        }
        let p = f.predict(500.0).unwrap();
        assert!((p - 7.0).abs() < 0.01, "{p}");
    }

    #[test]
    fn sustained_heavy_load_fans_out_then_back() {
        let mut g = ThreadGovernor::new(60); // 16.7 ms budget
        let mut t = 0u64;
        let mut got = None;
        for _ in 0..40 {
            g.want(4.0, 100.0, 4, t); // learn the one-thread cost
            t += 16;
        }
        for _ in 0..40 {
            if let Some(n) = g.want(15.0, 5000.0, 4, t) {
                got = Some(n);
                break;
            }
            t += 16;
        }
        assert_eq!(got, Some(4));
        g.switched(4);
        // Light again: back to one thread after 2 s.
        let mut back = None;
        for _ in 0..400 {
            if let Some(n) = g.want(2.0, 100.0, 4, t) {
                back = Some(n);
                break;
            }
            t += 16;
        }
        assert_eq!(back, Some(1));
    }
}
