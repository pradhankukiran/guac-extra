// SPDX-License-Identifier: Apache-2.0 AND MIT
// Derived from voxl src/video.rs (stripe_layout, Stripe, stripe_worker)
// (MIT, Copyright (c) 2026 Tembo).
//! Horizontal stripes, each an independent H.264 stream.
//!
//! x264 pays ~9 ms of CPU per 2560x1600 frame even when nothing changed, and
//! sliced threading triples that. Independent single-threaded stripes are
//! encoded in parallel (low latency) and only when they changed (low CPU).

use crate::convert::I420;
use crate::encoder::{Encoder, EncoderConfig, Planes};
use crate::governor::ThreadGovernor;
use crate::log::Logger;
use crate::quality::{MotionQuality, MotionSize};
use std::sync::{mpsc, Arc};
use std::time::Instant;

/// Splits `height` rows into up to `n` stripes whose heights are multiples
/// of `align` (16 or 32; except the last). Returns (y, height) pairs. With
/// 32, stripes still start on a macroblock row at half size.
pub fn layout_aligned(height: u32, n: u32, align: u32) -> Vec<(u32, u32)> {
    let n = n.clamp(1, 16);
    let each = (height.div_ceil(n) + align - 1) & !(align - 1);
    let mut v = Vec::new();
    let mut y = 0;
    while y < height {
        let h = each.min(height - y);
        v.push((y, h));
        y += h;
    }
    v
}

pub struct Stripe {
    pub y: usize,
    pub h: usize,
    enc: Encoder,
    cfg: EncoderConfig,
    /// Sliced threads to fan out to under sustained heavy motion (1 = never).
    max_threads: u32,
    applied_kbps: u32,
    applied_crf: f32,
}

impl Stripe {
    pub fn new(y: usize, cfg: EncoderConfig, max_threads: u32) -> Result<Self, String> {
        Ok(Stripe {
            y,
            h: cfg.height as usize,
            enc: Encoder::new(&cfg)?,
            cfg,
            max_threads,
            applied_kbps: 0,
            applied_crf: -1.0,
        })
    }
}

#[derive(Clone, Copy)]
struct SendPtr<T>(*const T);
unsafe impl<T> Send for SendPtr<T> {}

pub struct Job {
    /// Valid until the worker replies: the session waits for every stripe.
    buf: SendPtr<I420>,
    pub ts: i64,
    pub now_ms: u64,
    pub force_idr: bool,
    /// A paint-over or refinement pass (`crf` is the paint-over CRF).
    pub paintover: bool,
    pub crf: f32,
    pub cap_kbps: u32,
    pub mb_dirty: Option<Arc<Vec<u8>>>,
    pub mb_cols: usize,
    /// Where the user is working, in this picture's pixels (x, y, width,
    /// height): its macroblocks are encoded `focus_boost` QP better, the
    /// rest a quarter of that worse.
    pub focus: Option<(usize, usize, usize, usize)>,
    pub focus_boost: f32,
}

impl Job {
    /// # Safety
    /// `buf` must stay alive and unchanged until the worker has replied.
    pub unsafe fn new(buf: &I420) -> Self {
        Job {
            buf: SendPtr(buf as *const I420),
            ts: 0,
            now_ms: 0,
            force_idr: false,
            paintover: false,
            crf: 0.0,
            cap_kbps: 0,
            mb_dirty: None,
            mb_cols: 0,
            focus: None,
            focus_boost: 0.0,
        }
    }
}

/// (stripe index, Some((keyframe, Annex-B bytes)) or None if nothing came out).
pub type JobResult = (usize, Option<(bool, Vec<u8>)>);

/// Runs one stripe's encoder until `rx` closes.
pub fn worker(
    i: usize,
    mut s: Stripe,
    fps: u32,
    rx: mpsc::Receiver<Job>,
    done: mpsc::Sender<JobResult>,
    log: Logger,
) {
    let mut gov = ThreadGovernor::new(fps);
    let mut motion = MotionQuality::default();
    while let Ok(job) = rx.recv() {
        let buf = unsafe { &*job.buf.0 };
        let is_motion = !job.force_idr && !job.paintover;
        let crf = if is_motion { motion.crf(job.crf) } else { job.crf };
        if job.force_idr {
            // A keyframe is what a new viewer sees first; don't starve it to
            // the steady-state frame budget (up to ~8 frames at 4x the cap).
            let boost = job.cap_kbps.saturating_mul(4).max(16_000);
            s.enc.set_rate_buf(job.crf, boost, boost / fps.max(1) * 8);
            s.applied_kbps = 0;
        } else {
            let moved = s.applied_kbps == 0
                || (job.cap_kbps as f64 - s.applied_kbps as f64).abs()
                    > s.applied_kbps as f64 * 0.04;
            if crf != s.applied_crf || moved {
                s.enc.set_rate(crf, job.cap_kbps);
                s.applied_crf = crf;
                s.applied_kbps = job.cap_kbps;
            }
        }
        let suv = buf.stride_uv();
        let planes = Planes {
            y: &buf.y[s.y * buf.width..],
            u: &buf.u[(s.y / 2) * suv..],
            v: &buf.v[(s.y / 2) * suv..],
            stride_y: buf.width,
            stride_uv: suv,
        };
        let mut out = Vec::with_capacity(64 << 10);
        // x264 wants 1 (X264_MBINFO_CONSTANT) on unchanged macroblocks.
        let rows = s.h.div_ceil(16);
        let mut info = job.mb_dirty.as_ref().map(|d| {
            let start = (s.y / 16) * job.mb_cols;
            d[start..start + rows * job.mb_cols]
                .iter()
                .map(|&x| (x == 0) as u8)
                .collect::<Vec<u8>>()
        });
        let stripe_mbs = rows * job.mb_cols;
        let changed = info
            .as_ref()
            .map_or(stripe_mbs, |m| m.iter().filter(|&&c| c == 0).count());
        // Better quality around where the user is working
        let mut offsets = job.focus.filter(|_| job.focus_boost > 0.0).map(|(fx, fy, fw, fh)| {
            let (c0, c1) = (fx / 16, (fx + fw).div_ceil(16));
            (0..rows * job.mb_cols)
                .map(|i| {
                    let (col, row) = (i % job.mb_cols, s.y / 16 + i / job.mb_cols);
                    let y = row * 16;
                    let inside = col >= c0 && col < c1 && y + 16 > fy && y < fy + fh;
                    if inside { -job.focus_boost } else { job.focus_boost / 4.0 }
                })
                .collect::<Vec<f32>>()
        });
        let te = Instant::now();
        let r = s.enc.encode(
            &planes,
            job.ts,
            job.force_idr,
            info.as_deref_mut(),
            offsets.as_deref_mut(),
            &mut out,
        );
        let enc_ms = te.elapsed().as_secs_f64() * 1000.0;
        if s.max_threads > 1 {
            if let Some(t) = gov.want(enc_ms, changed as f64, s.max_threads, job.now_ms) {
                match Encoder::new(&EncoderConfig {
                    threads: t,
                    ..s.cfg.clone()
                }) {
                    Ok(e) => {
                        log.info(|| format!("stripe {i}: encoder now {t} thread(s)"));
                        // A fresh encoder opens with an IDR; rate control restarts.
                        s.enc = e;
                        motion = MotionQuality::default();
                        s.applied_kbps = 0;
                        gov.switched(t);
                    }
                    Err(e) => log.error(|| format!("stripe {i}: encoder reopen failed: {e}")),
                }
            }
        }
        let res = match r {
            Ok(e) => {
                let size = is_motion.then_some(MotionSize {
                    bits: out.len() * 8,
                    changed,
                    mbs: stripe_mbs,
                    crf: job.crf,
                });
                motion.note(&e, crf, size);
                (!out.is_empty()).then_some((e.keyframe, out))
            }
            Err(e) => {
                log.error(|| format!("stripe {i}: {e}"));
                None
            }
        };
        if done.send((i, res)).is_err() {
            return;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn aligned_layout_halves_onto_macroblock_rows() {
        let l = layout_aligned(1080, 4, 32);
        assert!(l.iter().all(|&(y, _)| y % 32 == 0), "{l:?}");
        assert_eq!(l.iter().map(|&(_, h)| h).sum::<u32>(), 1080);
    }

    #[test]
    fn layout_covers_height_in_16_row_multiples() {
        assert_eq!(layout_aligned(1080, 1, 16), vec![(0, 1080)]);
        let l = layout_aligned(1080, 4, 16);
        assert_eq!(l, vec![(0, 272), (272, 272), (544, 272), (816, 264)]);
        let l = layout_aligned(100, 16, 16);
        assert!(l.iter().all(|&(_, h)| h > 0));
        assert_eq!(l.iter().map(|&(_, h)| h).sum::<u32>(), 100);
    }
}
