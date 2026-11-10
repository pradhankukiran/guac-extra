// SPDX-License-Identifier: Apache-2.0 AND MIT
// Derived from voxl src/video.rs (capture_loop, encode_loop)
// (MIT, Copyright (c) 2026 Tembo).
//! One video stream for one display surface.
//!
//! The host (guacd) calls [`Session::update`] with changed pixels whenever
//! the display changes, and [`Session::encode`] whenever it could send a
//! frame. `encode` decides whether a frame is due (damage, keyframe requests,
//! paint-over timers, flow control), encodes the stripes that need it in
//! parallel, and returns the frame with the viewers it must go to.
//!
//! Unlike voxl, the session never reads a clock or captures the screen:
//! every call takes `now_ms` from the host, and pixels arrive from guacd.

use crate::convert::{convert_rows, scale, I420};
use crate::encoder::EncoderConfig;
use crate::flow::{flow_ok, Inflight, RateCtl, Viewer};
use crate::log::Logger;
use crate::refine::{Band, Refiner};
use crate::stripes::{self, Job, JobResult, Stripe};
use std::sync::{mpsc, Arc};
use std::thread::JoinHandle;

/// Encoder settings. Defaults follow voxl's.
#[derive(Clone, Debug)]
pub struct Config {
    pub fps: u32,
    pub preset: String,
    /// Encoder threads in total, split across stripes.
    pub threads: u32,
    pub convert_threads: usize,
    pub crf: f32,
    pub paintover_crf: f32,
    /// Paint-over frames sent after motion stops.
    pub paintover_burst: u32,
    pub paintover_delay_ms: u64,
    pub max_kbps: u32,
    pub min_kbps: u32,
    pub stripes: u32,
    /// One thread per stripe unless a frame would miss its budget.
    pub adaptive_threads: bool,
    /// Halve the video's resolution while the link can't carry even
    /// `min_kbps` (the viewer scales it back up).
    pub scaling: bool,
    /// QP steps of extra quality around the focus (see
    /// [`Session::set_focus`]); the rest gets a quarter of that less. 0
    /// turns it off.
    pub focus_boost: f32,
    /// ID of the first frame. Two encoders serving one display (simulcast)
    /// use IDs half the 32-bit range apart, so an ack for one never counts
    /// for the other.
    pub first_frame_id: u32,
}

impl Default for Config {
    fn default() -> Self {
        let ncpu = std::thread::available_parallelism().map_or(4, |n| n.get()) as u32;
        Config {
            fps: 60,
            preset: "ultrafast".into(),
            threads: (ncpu / 4).clamp(1, 4),
            convert_threads: 4,
            crf: 24.0,
            paintover_crf: 16.0,
            paintover_burst: 3,
            paintover_delay_ms: 150,
            max_kbps: 30_000,
            min_kbps: 2000,
            stripes: 1,
            adaptive_threads: false,
            scaling: false,
            focus_boost: 0.0,
            first_frame_id: 1,
        }
    }
}

/// A changed rectangle, in display pixels.
#[derive(Clone, Copy, Debug)]
pub struct Rect {
    pub x: i32,
    pub y: i32,
    pub width: i32,
    pub height: i32,
}

/// One stripe's H.264 output for a frame (Annex-B).
pub struct Part {
    pub stripe: u32,
    pub y: u32,
    pub height: u32,
    pub keyframe: bool,
    pub data: Vec<u8>,
}

pub struct Frame {
    pub id: u32,
    /// Encoded at paint-over quality.
    pub paintover: bool,
    /// Every stripe is a keyframe: a viewer can start decoding here.
    pub keyframe: bool,
    pub parts: Vec<Part>,
    /// Viewers this frame must be sent to (all of its parts).
    pub recipients: Vec<u32>,
}

pub struct Session {
    cfg: Config,
    log: Logger,
    /// Display size; the coded size is rounded up to even.
    width: usize,
    height: usize,
    buf: Box<I420>,
    mb_cols: usize,
    layout: Vec<(u32, u32)>,
    /// Stripe was encoded at motion quality since its last paint-over.
    touched: Vec<bool>,
    workers: Vec<(mpsc::Sender<Job>, JoinHandle<()>)>,
    done_rx: mpsc::Receiver<JobResult>,
    band: Option<Band>,
    mb_dirty: Vec<u8>,
    full_next: bool,
    refiner: Refiner,
    rc: RateCtl,
    viewers: Vec<Viewer>,
    next_viewer: u32,
    had_viewers: bool,
    frame_id: u32,
    last_motion: Option<u64>,
    paintover_left: u32,
    last_capture: Option<u64>,
    epoch: u64,
    frame: Option<Frame>,
    flow_blocked: bool,
    /// Where the user is working, in display pixels.
    focus: Option<Rect>,
    /// Resolution divisor: 1 (full) or 2 (half).
    scale: u32,
    /// The half-size picture while scaled.
    small: Option<Box<I420>>,
    scaler: Scaler,
}

/// When to switch resolution (see [`Session::update_scale`]).
#[derive(Default)]
struct Scaler {
    /// Since when the bitrate has sat at the floor with congestion.
    floor_since: Option<u64>,
    scaled_at: u64,
    upscaled_at: Option<u64>,
    /// How long to stay scaled before trying full resolution again.
    retry_ms: u64,
}

/// Floor with congestion this long: halve the resolution.
const SCALE_DOWN_AFTER_MS: u64 = 3_000;
/// First wait before trying full resolution again; doubles (up to the max)
/// each time full resolution congests again within `SCALE_FAILED_MS`.
const SCALE_RETRY_MS: u64 = 15_000;
const SCALE_RETRY_MAX_MS: u64 = 120_000;
const SCALE_FAILED_MS: u64 = 15_000;
/// No congestion for this long before trying full resolution.
const SCALE_QUIET_MS: u64 = 5_000;

type Workers = (Vec<(mpsc::Sender<Job>, JoinHandle<()>)>, mpsc::Receiver<JobResult>);

/// Even size of `full` divided by `scale`, rounded up.
fn scaled(full: u32, scale: u32) -> u32 {
    (full.div_ceil(scale) + 1) & !1
}

/// Starts one encoder thread per stripe, for pictures `scale` times smaller
/// than the coded size `cw` x `ch`.
fn start_workers(cfg: &Config, layout: &[(u32, u32)], cw: u32, ch: u32, scale: u32,
                 log: &Logger) -> Result<Workers, String> {
    let n = layout.len() as u32;
    let (sw, sh) = (scaled(cw, scale), scaled(ch, scale));
    let (done_tx, done_rx) = mpsc::channel();
    let mut workers = Vec::new();
    for (i, &(y, h)) in layout.iter().enumerate() {
        // Stripe rows in the scaled picture: from its (even) top to the next
        // stripe's top, or to the bottom for the last one.
        let top = (y / scale) & !1;
        let bottom = if i + 1 == layout.len() { sh } else { ((y + h) / scale) & !1 };
        let per = (cfg.threads / n).max(1);
        let ecfg = EncoderConfig {
            width: sw,
            height: bottom - top,
            fps: cfg.fps,
            preset: cfg.preset.clone(),
            threads: if cfg.adaptive_threads { 1 } else { per },
            crf: cfg.crf,
            max_kbps: cfg.max_kbps,
        };
        let max_threads = if cfg.adaptive_threads { per } else { 1 };
        let stripe = Stripe::new(top as usize, ecfg, max_threads)?;
        let (tx, rx) = mpsc::channel();
        let (done, log, fps) = (done_tx.clone(), log.clone(), cfg.fps);
        let handle = std::thread::Builder::new()
            .name(format!("gx-enc-{i}"))
            .spawn(move || stripes::worker(i, stripe, fps, rx, done, log))
            .map_err(|e| e.to_string())?;
        workers.push((tx, handle));
    }
    Ok((workers, done_rx))
}

/// Stops encoder threads.
fn stop_workers(workers: &mut Vec<(mpsc::Sender<Job>, JoinHandle<()>)>) {
    for (tx, handle) in workers.drain(..) {
        drop(tx);
        let _ = handle.join();
    }
}

/// A macroblock map for a picture `scale` times smaller: a small block is
/// marked if any full-size block it covers is.
fn shrink_map(map: &[u8], cols: usize, scale: usize, small_cols: usize, small_rows: usize) -> Vec<u8> {
    let rows = map.len() / cols;
    let mut out = vec![0u8; small_cols * small_rows];
    for sy in 0..small_rows {
        for sx in 0..small_cols {
            let mut any = 0;
            for y in sy * scale..((sy + 1) * scale).min(rows) {
                for x in sx * scale..((sx + 1) * scale).min(cols) {
                    any |= map[y * cols + x];
                }
            }
            out[sy * small_cols + sx] = any;
        }
    }
    out
}

impl Session {
    pub fn new(cfg: Config, width: u32, height: u32, now_ms: u64, log: Logger) -> Result<Self, String> {
        if width == 0 || height == 0 || width > 16384 || height > 16384 {
            return Err(format!("bad display size {width}x{height}"));
        }
        if cfg.fps == 0 || cfg.max_kbps == 0 {
            return Err("fps and max_kbps must be positive".into());
        }
        let (cw, ch) = ((width + 1) & !1, (height + 1) & !1);
        // At half resolution stripes must still start on a macroblock row
        let layout = stripes::layout_aligned(ch, cfg.stripes, if cfg.scaling { 32 } else { 16 });
        let (workers, done_rx) = start_workers(&cfg, &layout, cw, ch, 1, &log)?;
        let mb_cols = (cw as usize).div_ceil(16);
        let mb_rows = (ch as usize).div_ceil(16);
        log.info(|| {
            format!(
                "video session {width}x{height} @ {} fps, {} stripe(s), preset {}",
                cfg.fps,
                layout.len(),
                cfg.preset
            )
        });
        Ok(Session {
            rc: RateCtl::new(cfg.max_kbps as f64, cfg.min_kbps as f64),
            touched: vec![false; layout.len()],
            buf: Box::new(I420::new(cw as usize, ch as usize)),
            width: width as usize,
            height: height as usize,
            mb_cols,
            layout,
            workers,
            done_rx,
            band: None,
            mb_dirty: vec![0; mb_cols * mb_rows],
            full_next: true,
            refiner: Refiner::new(mb_cols * mb_rows, mb_cols, now_ms),
            viewers: Vec::new(),
            next_viewer: 1,
            had_viewers: false,
            frame_id: cfg.first_frame_id.wrapping_sub(1),
            last_motion: None,
            paintover_left: 0,
            last_capture: None,
            epoch: now_ms,
            frame: None,
            flow_blocked: false,
            focus: None,
            scale: 1,
            small: None,
            scaler: Scaler {
                retry_ms: SCALE_RETRY_MS,
                ..Default::default()
            },
            cfg,
            log,
        })
    }

    pub fn size(&self) -> (u32, u32) {
        (self.width as u32, self.height as u32)
    }

    /// Stripes as (top row, height) in coded pixels, top to bottom.
    pub fn stripes(&self) -> &[(u32, u32)] {
        &self.layout
    }

    pub fn target_kbps(&self) -> u32 {
        self.rc.target as u32
    }

    /// Sets where the user is working (e.g. around the mouse pointer), in
    /// display pixels; `None` for nowhere in particular. With `focus_boost`
    /// set, that area is encoded at better quality.
    pub fn set_focus(&mut self, focus: Option<Rect>) {
        self.focus = focus.filter(|r| r.width > 0 && r.height > 0);
    }

    /// Current resolution divisor: 1 (full) or 2 (half).
    pub fn scale(&self) -> u32 {
        self.scale
    }

    /// Switches to `scale` (1 or 2): new encoders at that size, and a
    /// keyframe for every viewer.
    fn set_scale(&mut self, scale: u32) -> Result<(), String> {
        let (cw, ch) = (self.buf.width as u32, self.buf.height as u32);
        stop_workers(&mut self.workers);
        let (workers, done_rx) = start_workers(&self.cfg, &self.layout, cw, ch, scale, &self.log)?;
        self.workers = workers;
        self.done_rx = done_rx;
        self.scale = scale;
        self.small = (scale > 1).then(|| {
            Box::new(I420::new(scaled(cw, scale) as usize, scaled(ch, scale) as usize))
        });
        self.touched.fill(false);
        for v in &mut self.viewers {
            v.needs_keyframe = true;
        }
        // A picture `scale` times smaller in each direction needs about
        // scale^2 times fewer bits: let the bitrate follow the link down
        let floor = self.cfg.min_kbps as f64 / (scale * scale) as f64;
        self.rc.set_min(floor);
        if scale > 1 {
            self.log.info(|| format!(
                "the link can't carry {} kbit/s: video at 1/{scale} resolution ({}x{})",
                self.cfg.min_kbps, scaled(cw, scale), scaled(ch, scale)));
        } else {
            self.log.info(|| "video back at full resolution".to_string());
        }
        Ok(())
    }

    /// Halves the resolution after the bitrate has sat at its floor with
    /// congestion for 3 s (the floor then drops to a quarter). Tries full
    /// resolution again after 5 s without congestion, once the half-size
    /// bitrate has climbed back to the full-size floor (after 15 s), or in
    /// any case after 60 s. Each failed try doubles both waits.
    fn update_scale(&mut self, now: u64) -> Result<(), String> {
        if !self.cfg.scaling {
            return Ok(());
        }
        let since = |t: u64| now.saturating_sub(t);
        let congested = self.rc.last_congested();
        if self.scale == 1 {
            if self.rc.at_floor() && congested.is_some_and(|t| since(t) < 1_000) {
                let start = *self.scaler.floor_since.get_or_insert(now);
                if since(start) >= SCALE_DOWN_AFTER_MS {
                    // Full resolution failed again soon after a retry
                    if self.scaler.upscaled_at.is_some_and(|t| since(t) < SCALE_FAILED_MS) {
                        self.scaler.retry_ms = (self.scaler.retry_ms * 2).min(SCALE_RETRY_MAX_MS);
                    }
                    self.scaler.floor_since = None;
                    self.scaler.scaled_at = now;
                    self.set_scale(2)?;
                }
            } else {
                self.scaler.floor_since = None;
                if self.scaler.upscaled_at.is_some_and(|t| since(t) >= 2 * SCALE_FAILED_MS) {
                    self.scaler.retry_ms = SCALE_RETRY_MS;
                }
            }
        } else if congested.is_none_or(|t| since(t) >= SCALE_QUIET_MS)
            && (
                // The half-size bitrate climbed back to the full-size floor:
                // full resolution will likely fit
                (self.rc.target >= self.cfg.min_kbps as f64
                    && since(self.scaler.scaled_at) >= self.scaler.retry_ms)
                // Light content may never push the bitrate up: try anyway,
                // rarely, so a link that got faster is found
                || since(self.scaler.scaled_at) >= self.scaler.retry_ms * 4
            )
        {
            self.scaler.upscaled_at = Some(now);
            self.set_scale(1)?;
        }
        Ok(())
    }

    fn full_band(&self) -> Band {
        Band {
            y0: 0,
            y1: self.buf.height as u32,
        }
    }

    /// Copies the changed rectangles of a BGRX image (the whole display,
    /// `stride` bytes per row) into the session.
    ///
    /// # Safety
    /// `pixels` must point to `height` rows of `stride` bytes, each holding
    /// `width` 32-bit pixels, for the display size given to [`Session::new`].
    pub unsafe fn update(&mut self, pixels: *const u8, stride: usize, rects: &[Rect]) {
        let (w, h) = (self.width as i32, self.height as i32);
        let mut changed: Option<Band> = None;
        for r in rects {
            let (x0, y0) = (r.x.clamp(0, w), r.y.clamp(0, h));
            let x1 = r.x.saturating_add(r.width).clamp(0, w);
            let y1 = r.y.saturating_add(r.height).clamp(0, h);
            if x1 <= x0 || y1 <= y0 {
                continue;
            }
            let (c0, c1) = (x0 as usize / 16, (x1 as usize).div_ceil(16));
            for row in (y0 as usize / 16)..(y1 as usize).div_ceil(16) {
                self.mb_dirty[row * self.mb_cols + c0..row * self.mb_cols + c1].fill(1);
            }
            changed = Some(Band::union(changed, Band { y0: y0 as u32, y1: y1 as u32 }));
        }
        let Some(b) = changed else {
            return;
        };
        // 4:2:0 needs whole row pairs.
        let (y0, y1) = ((b.y0 & !1) as usize, ((b.y1 + 1) & !1) as usize);
        convert_rows(
            pixels,
            stride,
            self.width,
            self.height,
            &mut self.buf,
            y0,
            y1,
            self.cfg.convert_threads,
        );
        self.band = Some(Band::union(self.band, b));
    }

    pub fn add_viewer(&mut self, now_ms: u64) -> u32 {
        let id = self.next_viewer;
        self.next_viewer = self.next_viewer.wrapping_add(1).max(1);
        self.viewers.push(Viewer::new(id, now_ms));
        id
    }

    pub fn remove_viewer(&mut self, id: u32) {
        self.viewers.retain(|v| v.id != id);
    }

    fn viewer(&mut self, id: u32) -> Option<&mut Viewer> {
        self.viewers.iter_mut().find(|v| v.id == id)
    }

    pub fn ack(&mut self, viewer: u32, frame_id: u32, now_ms: u64) {
        if let Some(v) = self.viewer(viewer) {
            v.on_ack(frame_id, now_ms);
        }
    }

    pub fn rtt(&mut self, viewer: u32, rtt_ms: u32) {
        if let Some(v) = self.viewer(viewer) {
            v.on_rtt(rtt_ms);
        }
    }

    /// Whether a viewer's own deliveries show congestion right now: acks
    /// queueing more than 30 ms over its base delay, or its oldest
    /// unacknowledged frame older than RTT + 80 ms (the rate controller's
    /// own test, for one viewer).
    pub fn viewer_congested(&self, viewer: u32, now_ms: u64) -> bool {
        self.viewers.iter().find(|v| v.id == viewer).is_some_and(|v| {
            let (_, age) = v.backlog(now_ms);
            v.started && (v.queue_ms > 30.0 || age as f64 > v.rtt_or_default() + 80.0)
        })
    }

    pub fn request_keyframe(&mut self, viewer: u32) {
        if let Some(v) = self.viewer(viewer) {
            v.needs_keyframe = true;
        }
    }

    fn min_interval_ms(&self) -> f64 {
        // Allow slightly early frames so source jitter doesn't drop frames.
        1000.0 / self.cfg.fps as f64 * 0.85
    }

    fn since(t: Option<u64>, now: u64) -> u64 {
        t.map_or(u64::MAX, |t| now.saturating_sub(t))
    }

    /// Milliseconds until [`Session::encode`] should be called again if
    /// nothing else happens, or -1 if only new damage, acks or viewers can
    /// make a frame due.
    pub fn next_deadline_ms(&self, now_ms: u64) -> i64 {
        if self.viewers.is_empty() {
            return -1;
        }
        let want_key = self.viewers.iter().any(|v| v.needs_keyframe);
        let mut next = i64::MAX;
        if self.band.is_some() || want_key {
            let wait = self.min_interval_ms() - Self::since(self.last_capture, now_ms) as f64;
            next = next.min(wait.ceil().max(0.0) as i64);
            // Held back by flow control: re-check every 4 ms, as voxl does.
            if self.flow_blocked {
                next = next.max(4);
            }
        }
        if self.paintover_left > 0 {
            let wait = self.cfg.paintover_delay_ms as i64
                - Self::since(self.last_motion, now_ms).min(i64::MAX as u64) as i64;
            next = next.min(wait.max(0));
        }
        if next == i64::MAX {
            -1
        } else {
            next
        }
    }

    /// The frame produced by the last successful [`Session::encode`].
    pub fn frame(&self) -> Option<&Frame> {
        self.frame.as_ref()
    }

    /// Encodes a frame if one is due. Returns Ok(true) when [`Session::frame`]
    /// holds a new frame to send.
    pub fn encode(&mut self, now: u64) -> Result<bool, String> {
        if self.viewers.is_empty() {
            if self.had_viewers {
                self.rc = RateCtl::new(self.cfg.max_kbps as f64, self.cfg.min_kbps as f64);
            }
            self.had_viewers = false;
            return Ok(false);
        }
        self.rc.update(&self.viewers, now, &self.log);
        self.update_scale(now)?;
        let want_key = self.viewers.iter().any(|v| v.needs_keyframe);
        if !self.had_viewers {
            self.had_viewers = true;
            self.band = Some(self.full_band());
            self.full_next = true;
        }
        let paintover_due = self.paintover_left > 0
            && Self::since(self.last_motion, now) >= self.cfg.paintover_delay_ms;
        if self.band.is_none() && !want_key && !paintover_due {
            return Ok(false);
        }
        if (Self::since(self.last_capture, now) as f64) < self.min_interval_ms() {
            return Ok(false);
        }
        let interval_ms = 1000.0 / self.cfg.fps as f64;
        self.flow_blocked = !want_key && !flow_ok(&mut self.viewers, interval_ms, now, &self.log);
        if self.flow_blocked {
            return Ok(false);
        }

        let mut mb_dirty = if self.full_next {
            self.mb_dirty.fill(0);
            self.full_next = false;
            None
        } else {
            let n = self.mb_dirty.len();
            Some(std::mem::replace(&mut self.mb_dirty, vec![0; n]))
        };
        match &mb_dirty {
            Some(m) => self.refiner.mark(m, now),
            None => self.refiner.mark_all(now),
        }
        let mut band = self.band.take();
        let is_motion = band.is_some();
        // Sharpen only what was sent at motion quality.
        let paint_map = if paintover_due && !is_motion && !want_key {
            match self.refiner.pending_map() {
                Some(m) => Some(m),
                None => {
                    self.paintover_left = 0;
                    return Ok(false);
                }
            }
        } else {
            None
        };

        let mut refine = None;
        let paintover = if is_motion {
            self.last_motion = Some(now);
            self.paintover_left = self.cfg.paintover_burst;
            // Piggyback sharpening of blocks that have gone still.
            // paintover_burst 0 turns sharpening off entirely (guacd's
            // lossless overlay shows still areas exactly instead)
            refine = if want_key || self.cfg.paintover_burst == 0 {
                None
            } else {
                self.refiner.due(now)
            };
            refine.is_some()
        } else if paintover_due {
            self.paintover_left -= 1;
            true
        } else {
            false
        };
        if want_key {
            for v in &mut self.viewers {
                v.needs_keyframe = false;
            }
            // A keyframe is squeezed into the rate cap; sharpen it once still.
            self.refiner.mark_all(now);
        }
        let frame_paintover = paintover && refine.is_none();
        if frame_paintover && self.paintover_left == 0 {
            self.refiner.clear_all();
        }
        // A refinement frame encodes what moved plus what is due.
        let mb_map = if want_key {
            None
        } else if frame_paintover {
            paint_map.map(|(m, b)| {
                band = Some(Band::union(band, b));
                Arc::new(m)
            })
        } else if let Some((rmap, rband)) = refine.take() {
            band = Some(Band::union(band, rband));
            let mut m = mb_dirty.take().unwrap_or_else(|| vec![1; rmap.len()]);
            for (d, r) in m.iter_mut().zip(rmap) {
                *d |= r;
            }
            Some(Arc::new(m))
        } else {
            mb_dirty.take().map(Arc::new)
        };
        self.last_capture = Some(now);
        let paintover_final = frame_paintover && self.paintover_left == 0;
        self.encode_stripes(now, want_key, paintover, paintover_final, band, mb_map)
    }

    fn encode_stripes(
        &mut self,
        now: u64,
        force_idr: bool,
        paintover: bool,
        paintover_final: bool,
        band: Option<Band>,
        mb_map: Option<Arc<Vec<u8>>>,
    ) -> Result<bool, String> {
        let n = self.layout.len();
        // Whether the macroblock map asks for anything in rows [y, y + h).
        // voxl re-encodes every touched stripe on a paint-over pass; a stripe
        // with nothing left to sharpen would only produce an all-skip frame.
        let cols = self.mb_cols;
        let wants = |y: usize, h: usize| match &mb_map {
            None => true,
            Some(m) => m[(y / 16) * cols..((y + h).div_ceil(16) * cols).min(m.len())]
                .iter()
                .any(|&d| d != 0),
        };
        let selected: Vec<bool> = (0..n)
            .map(|i| {
                let (y, h) = (self.layout[i].0 as usize, self.layout[i].1 as usize);
                force_idr
                    || (paintover && self.touched[i] && wants(y, h))
                    || band.is_some_and(|b| b.overlaps(y, h))
            })
            .collect();
        if !selected.iter().any(|&s| s) {
            return Ok(false);
        }
        // At half resolution, encoders read the half-size picture and a
        // macroblock map for it.
        let (mb_map, job_cols) = match &mut self.small {
            Some(small) => {
                scale(&self.buf, small);
                let s = self.scale as usize;
                let (sc, sr) = (small.width.div_ceil(16), small.height.div_ceil(16));
                let m = mb_map.map(|m| Arc::new(shrink_map(&m, self.mb_cols, s, sc, sr)));
                (m, sc)
            }
            None => (mb_map, self.mb_cols),
        };
        let target = self.rc.target as u64;
        let crf = if paintover { self.cfg.paintover_crf } else { self.cfg.crf };
        // Each stripe may use up to twice its area share of the budget; the
        // rate controller reins the total in if several move at once.
        let share = ((target * 2) / n as u64).max(target / n as u64 + 1) as u32;
        let mut pending = 0;
        for (i, &on) in selected.iter().enumerate() {
            if !on {
                continue;
            }
            // SAFETY: we wait below for every job before touching either buffer.
            let picture: &I420 = self.small.as_deref().unwrap_or(&self.buf);
            let mut job = unsafe { Job::new(picture) };
            job.ts = now.saturating_sub(self.epoch) as i64;
            job.now_ms = now;
            job.force_idr = force_idr;
            job.paintover = paintover;
            job.crf = crf;
            job.cap_kbps = if paintover { share.saturating_mul(2) } else { share };
            job.mb_dirty = mb_map.clone();
            job.mb_cols = job_cols;
            job.focus_boost = self.cfg.focus_boost;
            job.focus = self.focus.map(|r| {
                let s = self.scale as i32;
                let clamp = |v: i32| (v.max(0) / s) as usize;
                (clamp(r.x), clamp(r.y), clamp(r.width), clamp(r.height))
            });
            self.workers[i]
                .0
                .send(job)
                .map_err(|_| format!("stripe {i} encoder thread gone"))?;
            pending += 1;
        }
        let mut results: Vec<Option<(bool, Vec<u8>)>> = (0..n).map(|_| None).collect();
        for _ in 0..pending {
            let (i, r) = self
                .done_rx
                .recv()
                .map_err(|_| "encoder thread gone".to_string())?;
            results[i] = r;
        }
        for (i, t) in self.touched.iter_mut().enumerate() {
            if paintover_final {
                *t = false;
            }
            let (y, h) = (self.layout[i].0 as usize, self.layout[i].1 as usize);
            if band.is_some_and(|b| b.overlaps(y, h)) {
                *t = true;
            }
        }
        let parts: Vec<Part> = results
            .into_iter()
            .enumerate()
            .filter_map(|(i, r)| {
                r.map(|(keyframe, data)| Part {
                    stripe: i as u32,
                    y: self.layout[i].0,
                    height: self.layout[i].1,
                    keyframe,
                    data,
                })
            })
            .collect();
        if parts.is_empty() {
            return Ok(false);
        }
        let keyframe = parts.len() == n && parts.iter().all(|p| p.keyframe);
        let bytes: usize = parts.iter().map(|p| p.data.len()).sum();
        self.frame_id = self.frame_id.wrapping_add(1);
        let mut recipients = Vec::new();
        for v in &mut self.viewers {
            if !v.started {
                if !keyframe {
                    continue;
                }
                v.started = true;
                v.inflight.clear();
            }
            if v.inflight.len() > 600 {
                self.log
                    .warn(|| format!("viewer {} hopelessly behind; resyncing", v.id));
                v.resync();
                continue;
            }
            v.inflight.push_back(Inflight {
                id: self.frame_id,
                sent_ms: now,
                bytes,
            });
            recipients.push(v.id);
        }
        self.frame = Some(Frame {
            id: self.frame_id,
            paintover,
            keyframe,
            parts,
            recipients,
        });
        Ok(true)
    }
}

impl Drop for Session {
    fn drop(&mut self) {
        stop_workers(&mut self.workers);
    }
}
