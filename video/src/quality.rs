// SPDX-License-Identifier: Apache-2.0 AND MIT
// Derived from voxl src/video.rs MotionQuality (MIT, Copyright (c) 2026 Tembo).
//! Keeps cheap motion (window drags, scrolling) at the configured quality.
//!
//! x264's CRF raises the QP of frames its quick estimate judges complex, and
//! that estimate can't follow a dragged window or a fast scroll: the moved
//! area counts as new pixels and the QP lands 7-10 steps above the CRF,
//! although the real search codes the move as cheap motion vectors. Motion
//! frames that come out that cheap get a CRF lowered by what the estimate
//! adds. Video and new content really are complex and are left alone.

use crate::encoder::Encoded;

/// Most QP steps taken off a motion frame's CRF.
const MAX_MOTION_EXCESS: f32 = 12.0;

/// Bits per changed macroblock (at the configured CRF) under which motion
/// counts as cheap: a dragged window or a scrolled page costs 10-30, video
/// and new content 50 and more.
const CHEAP_MB_BITS: f32 = 40.0;

#[derive(Default)]
pub struct MotionQuality {
    /// QP steps x264's complexity estimate adds to the next frame.
    complexity: f32,
    /// log2 of bits per changed macroblock of recent motion frames, scaled to
    /// the configured CRF, averaged in the log domain.
    mb_bits_log2: f32,
    /// QP steps taken off the CRF of motion frames now.
    applied: f32,
}

/// How big a motion frame came out.
#[derive(Clone, Copy)]
pub struct MotionSize {
    pub bits: usize,
    /// Macroblocks that changed, of `mbs`.
    pub changed: usize,
    pub mbs: usize,
    /// The configured CRF.
    pub crf: f32,
}

impl MotionQuality {
    /// The CRF to encode a motion frame with, for the configured `crf`.
    pub fn crf(&mut self, crf: f32) -> f32 {
        let want = if self.mb_bits_log2 < CHEAP_MB_BITS.log2() {
            self.complexity.clamp(0.0, MAX_MOTION_EXCESS)
        } else {
            0.0
        };
        // Whole steps, and only once the estimate moved a full step: every
        // change reconfigures x264.
        if (want - self.applied).abs() >= 1.0 {
            self.applied = want.round();
        }
        (crf - self.applied).max(0.0)
    }

    /// Notes what x264 made of a frame encoded with `crf`. `size` is set for
    /// motion frames (not paint-overs).
    pub fn note(&mut self, e: &Encoded, crf: f32, size: Option<MotionSize>) {
        // A keyframe's QP isn't picked from the complexity estimate.
        let (false, Some(qp)) = (e.keyframe, e.qp) else {
            return;
        };
        self.observe(crf, qp, e);
        // Small updates cost more per macroblock and never raise x264's QP;
        // only big ones tell what kind of motion this is.
        if let Some(m) = size.filter(|m| m.changed * 16 >= m.mbs) {
            // Halving the quantizer step (6 QP) about doubles the bits.
            let b = (m.bits.max(1) as f32 / m.changed.max(1) as f32).log2() + (qp - m.crf) / 6.0;
            self.mb_bits_log2 = self.mb_bits_log2 * 0.7 + b * 0.3;
        }
    }

    /// x264 starts from QP0 = crf + complexity; VBV may raise it before the
    /// frame (to `qp`) and per row within it. So the complexity lies between
    /// `qp - crf_avg` and `qp - crf`, give or take rounding. Keep the last
    /// estimate while it fits, else move it to the nearest end.
    fn observe(&mut self, crf: f32, qp: f32, e: &Encoded) {
        let lo = qp - 0.5 - e.crf_avg;
        let hi = qp + 0.5 - crf;
        self.complexity = if hi - lo < 1.25 {
            (lo + hi) / 2.0
        } else {
            self.complexity.max(lo).min(hi)
        };
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn enc(qp: f32, crf_avg: f32) -> Encoded {
        Encoded { keyframe: false, qp: Some(qp), crf_avg }
    }

    #[test]
    fn cheap_motion_gets_lower_crf() {
        let mut m = MotionQuality::default();
        // x264 started 8 QP above CRF 24, and the frame was cheap: 8 bits per
        // macroblock at QP 32 is ~20 at the configured CRF, under the 40 limit.
        for _ in 0..10 {
            let size = MotionSize { bits: 8 * 1000, changed: 1000, mbs: 1000, crf: 24.0 };
            m.note(&enc(32.0, 24.0), 24.0, Some(size));
        }
        assert_eq!(m.crf(24.0), 16.0);
    }

    #[test]
    fn expensive_motion_is_left_alone() {
        let mut m = MotionQuality::default();
        for _ in 0..10 {
            let size = MotionSize { bits: 200 * 1000, changed: 1000, mbs: 1000, crf: 24.0 };
            m.note(&enc(32.0, 24.0), 24.0, Some(size));
        }
        assert_eq!(m.crf(24.0), 24.0);
    }

    #[test]
    fn keyframes_are_ignored() {
        let mut m = MotionQuality::default();
        let e = Encoded { keyframe: true, qp: Some(40.0), crf_avg: 24.0 };
        m.note(&e, 24.0, None);
        assert_eq!(m.complexity, 0.0);
    }
}
