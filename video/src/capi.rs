// SPDX-License-Identifier: Apache-2.0
//! C API for guacd. See include/guac_extra_video.h (generated).
//!
//! Threading: one GxEncoder must not be used from two threads at once. The
//! log callback may be called from the library's encoder threads.
//!
//! Time: every function that needs the time takes `now_ms`, a monotonic
//! millisecond clock owned by the caller (e.g. guac_timestamp_current()).

use crate::log::{GxLogLevel, Logger};
use crate::session::{Config, Rect, Session};
use std::ffi::{c_char, c_void, CStr, CString};

/// Version of this library, as a NUL-terminated string.
const VERSION: &str = concat!(env!("CARGO_PKG_VERSION"), "\0");

/// Log callback: `data` is GxConfig.log_data, `msg` is valid only during the call.
pub type GxLogFn = Option<unsafe extern "C" fn(data: *mut c_void, level: GxLogLevel, msg: *const c_char)>;

/// Encoder settings. Fill with gx_config_default(), then change fields.
#[repr(C)]
pub struct GxConfig {
    /// Maximum frames per second.
    pub fps: u32,
    /// x264 preset name, or NULL for "ultrafast".
    pub preset: *const c_char,
    /// Encoder threads in total, split across stripes.
    pub threads: u32,
    /// Threads for color conversion.
    pub convert_threads: u32,
    /// Quality while content moves (lower is better; 0-51).
    pub crf: f32,
    /// Quality for areas re-sent after they stop moving.
    pub paintover_crf: f32,
    /// Paint-over frames sent after motion stops. 0 turns sharpening of
    /// still areas off entirely.
    pub paintover_burst: u32,
    /// Delay after the last motion before paint-over starts.
    pub paintover_delay_ms: u32,
    /// Bitrate ceiling and floor for adaptive bitrate (kbit/s).
    pub max_kbps: u32,
    pub min_kbps: u32,
    /// Horizontal stripes, each its own H.264 stream (1-16).
    pub stripes: u32,
    /// One thread per stripe unless a frame would miss its time budget.
    pub adaptive_threads: bool,
    /// Halve the video's resolution while the link can't carry even
    /// `min_kbps`; viewers scale it back up.
    pub scaling: bool,
    /// QP steps of extra quality around the focus set with
    /// gx_encoder_set_focus() (the rest gets a quarter of that less); 0
    /// (default) turns it off.
    pub focus_boost: f32,
    /// ID of the first frame (default 1). A second encoder for the same
    /// viewers' streams (simulcast) should start half the range away.
    pub first_frame_id: u32,
    pub log: GxLogFn,
    pub log_data: *mut c_void,
}

/// A changed rectangle, in display pixels.
#[repr(C)]
#[derive(Clone, Copy)]
pub struct GxRect {
    pub x: i32,
    pub y: i32,
    pub width: i32,
    pub height: i32,
}

/// One stripe of the current frame. `data` stays valid until the next
/// gx_encoder_encode() or gx_encoder_free() on the same encoder.
#[repr(C)]
pub struct GxPart {
    /// Stripe index; each stripe is an independent H.264 stream.
    pub stripe: u32,
    /// Top row and height of the stripe, in coded pixels.
    pub y: u32,
    pub height: u32,
    /// This part is an IDR frame: a decoder for this stripe can start here.
    pub keyframe: bool,
    /// H.264 Annex-B bytes.
    pub data: *const u8,
    pub len: usize,
}

/// Opaque encoder handle.
pub struct GxEncoder {
    session: Session,
}

struct LogSink {
    f: unsafe extern "C" fn(*mut c_void, GxLogLevel, *const c_char),
    data: *mut c_void,
}
// The caller promises the callback and its data are usable from any thread.
unsafe impl Send for LogSink {}
unsafe impl Sync for LogSink {}

impl LogSink {
    fn call(&self, level: GxLogLevel, msg: &str) {
        let msg = CString::new(msg.replace('\0', " ")).unwrap_or_default();
        unsafe { (self.f)(self.data, level, msg.as_ptr()) }
    }
}

/// Returns the library version as a static, NUL-terminated string.
/// The caller must not free it.
#[no_mangle]
pub extern "C" fn gx_version() -> *const c_char {
    VERSION.as_ptr().cast()
}

/// Fills `cfg` with the default settings.
///
/// # Safety
/// `cfg` must point to writable memory for a GxConfig.
#[no_mangle]
pub unsafe extern "C" fn gx_config_default(cfg: *mut GxConfig) {
    if cfg.is_null() {
        return;
    }
    let d = Config::default();
    cfg.write(GxConfig {
        fps: d.fps,
        preset: std::ptr::null(),
        threads: d.threads,
        convert_threads: d.convert_threads as u32,
        crf: d.crf,
        paintover_crf: d.paintover_crf,
        paintover_burst: d.paintover_burst,
        paintover_delay_ms: d.paintover_delay_ms as u32,
        max_kbps: d.max_kbps,
        min_kbps: d.min_kbps,
        stripes: d.stripes,
        adaptive_threads: d.adaptive_threads,
        scaling: d.scaling,
        focus_boost: d.focus_boost,
        first_frame_id: d.first_frame_id,
        log: None,
        log_data: std::ptr::null_mut(),
    });
}

unsafe fn logger(cfg: &GxConfig) -> Logger {
    match cfg.log {
        None => Logger::default(),
        Some(f) => {
            let sink = LogSink { f, data: cfg.log_data };
            Logger::new(move |level, msg| sink.call(level, msg))
        }
    }
}

/// Creates an encoder for a display of `width` x `height` pixels. Returns
/// NULL on failure (the reason goes to the log callback). Create a new
/// encoder when the display size changes.
///
/// # Safety
/// `cfg` must point to a valid GxConfig; `preset`, if set, to a C string.
#[no_mangle]
pub unsafe extern "C" fn gx_encoder_new(
    cfg: *const GxConfig,
    width: u32,
    height: u32,
    now_ms: u64,
) -> *mut GxEncoder {
    let Some(c) = cfg.as_ref() else {
        return std::ptr::null_mut();
    };
    let log = logger(c);
    let preset = if c.preset.is_null() {
        "ultrafast".to_string()
    } else {
        CStr::from_ptr(c.preset).to_string_lossy().into_owned()
    };
    let config = Config {
        fps: c.fps,
        preset,
        threads: c.threads.max(1),
        convert_threads: c.convert_threads.max(1) as usize,
        crf: c.crf,
        paintover_crf: c.paintover_crf,
        paintover_burst: c.paintover_burst,
        paintover_delay_ms: c.paintover_delay_ms as u64,
        max_kbps: c.max_kbps,
        min_kbps: c.min_kbps,
        stripes: c.stripes,
        adaptive_threads: c.adaptive_threads,
        scaling: c.scaling,
        focus_boost: c.focus_boost,
        first_frame_id: c.first_frame_id,
    };
    match Session::new(config, width, height, now_ms, log.clone()) {
        Ok(session) => Box::into_raw(Box::new(GxEncoder { session })),
        Err(e) => {
            log.error(|| format!("cannot create video encoder: {e}"));
            std::ptr::null_mut()
        }
    }
}

/// Frees an encoder and stops its threads. NULL is ignored.
///
/// # Safety
/// `enc` must come from gx_encoder_new() and not be used afterwards.
#[no_mangle]
pub unsafe extern "C" fn gx_encoder_free(enc: *mut GxEncoder) {
    if !enc.is_null() {
        drop(Box::from_raw(enc));
    }
}

/// Copies the changed rectangles of the display into the encoder.
/// `bgrx` is the whole display, `stride` bytes per row, 32 bits per pixel
/// (Cairo's RGB24/ARGB32 layout on little-endian machines).
///
/// # Safety
/// `bgrx` must hold the full display at the size given to gx_encoder_new();
/// `rects` must point to `n_rects` GxRects.
#[no_mangle]
pub unsafe extern "C" fn gx_encoder_update(
    enc: *mut GxEncoder,
    bgrx: *const u8,
    stride: usize,
    rects: *const GxRect,
    n_rects: usize,
) {
    let (Some(e), false, false) = (enc.as_mut(), bgrx.is_null(), rects.is_null()) else {
        return;
    };
    let rects: Vec<Rect> = std::slice::from_raw_parts(rects, n_rects)
        .iter()
        .map(|r| Rect {
            x: r.x,
            y: r.y,
            width: r.width,
            height: r.height,
        })
        .collect();
    e.session.update(bgrx, stride, &rects);
}

/// Encodes a frame if one is due. Returns 1 if a new frame is ready (read it
/// with the gx_encoder_frame_* functions), 0 if not, -1 on error.
///
/// # Safety
/// `enc` must be a valid encoder.
#[no_mangle]
pub unsafe extern "C" fn gx_encoder_encode(enc: *mut GxEncoder, now_ms: u64) -> i32 {
    let Some(e) = enc.as_mut() else {
        return -1;
    };
    match e.session.encode(now_ms) {
        Ok(true) => 1,
        Ok(false) => 0,
        Err(_) => -1,
    }
}

/// Milliseconds until gx_encoder_encode() should be called again if nothing
/// else happens, or -1 if only new damage, acks or viewers can make a frame due.
///
/// # Safety
/// `enc` must be a valid encoder.
#[no_mangle]
pub unsafe extern "C" fn gx_encoder_next_deadline_ms(enc: *const GxEncoder, now_ms: u64) -> i64 {
    enc.as_ref().map_or(-1, |e| e.session.next_deadline_ms(now_ms))
}

/// ID of the current frame (what viewers acknowledge), or 0 if none.
///
/// # Safety
/// `enc` must be a valid encoder.
#[no_mangle]
pub unsafe extern "C" fn gx_encoder_frame_id(enc: *const GxEncoder) -> u32 {
    enc.as_ref()
        .and_then(|e| e.session.frame())
        .map_or(0, |f| f.id)
}

/// Whether every stripe of the current frame is a keyframe.
///
/// # Safety
/// `enc` must be a valid encoder.
#[no_mangle]
pub unsafe extern "C" fn gx_encoder_frame_keyframe(enc: *const GxEncoder) -> bool {
    enc.as_ref()
        .and_then(|e| e.session.frame())
        .is_some_and(|f| f.keyframe)
}

/// Whether the current frame re-sends still areas at paint-over quality.
///
/// # Safety
/// `enc` must be a valid encoder.
#[no_mangle]
pub unsafe extern "C" fn gx_encoder_frame_paintover(enc: *const GxEncoder) -> bool {
    enc.as_ref()
        .and_then(|e| e.session.frame())
        .is_some_and(|f| f.paintover)
}

/// Number of parts (stripes) in the current frame.
///
/// # Safety
/// `enc` must be a valid encoder.
#[no_mangle]
pub unsafe extern "C" fn gx_encoder_frame_parts(enc: *const GxEncoder) -> usize {
    enc.as_ref()
        .and_then(|e| e.session.frame())
        .map_or(0, |f| f.parts.len())
}

/// Reads part `i` of the current frame into `out`. Returns false if there is
/// no such part.
///
/// # Safety
/// `enc` must be a valid encoder; `out` must point to a writable GxPart.
#[no_mangle]
pub unsafe extern "C" fn gx_encoder_frame_part(
    enc: *const GxEncoder,
    i: usize,
    out: *mut GxPart,
) -> bool {
    let Some(p) = enc
        .as_ref()
        .and_then(|e| e.session.frame())
        .and_then(|f| f.parts.get(i))
    else {
        return false;
    };
    if out.is_null() {
        return false;
    }
    out.write(GxPart {
        stripe: p.stripe,
        y: p.y,
        height: p.height,
        keyframe: p.keyframe,
        data: p.data.as_ptr(),
        len: p.data.len(),
    });
    true
}

/// Writes up to `max` viewer IDs the current frame must be sent to into
/// `out`, and returns how many there are in total.
///
/// # Safety
/// `enc` must be a valid encoder; `out` must have room for `max` IDs.
#[no_mangle]
pub unsafe extern "C" fn gx_encoder_frame_recipients(
    enc: *const GxEncoder,
    out: *mut u32,
    max: usize,
) -> usize {
    let Some(f) = enc.as_ref().and_then(|e| e.session.frame()) else {
        return 0;
    };
    if !out.is_null() {
        for (i, id) in f.recipients.iter().take(max).enumerate() {
            out.add(i).write(*id);
        }
    }
    f.recipients.len()
}

/// Adds a viewer and returns its ID (never 0). New viewers get a keyframe.
///
/// # Safety
/// `enc` must be a valid encoder.
#[no_mangle]
pub unsafe extern "C" fn gx_encoder_add_viewer(enc: *mut GxEncoder, now_ms: u64) -> u32 {
    enc.as_mut().map_or(0, |e| e.session.add_viewer(now_ms))
}

/// Removes a viewer. Unknown IDs are ignored.
///
/// # Safety
/// `enc` must be a valid encoder.
#[no_mangle]
pub unsafe extern "C" fn gx_encoder_remove_viewer(enc: *mut GxEncoder, viewer: u32) {
    if let Some(e) = enc.as_mut() {
        e.session.remove_viewer(viewer);
    }
}

/// Notes that `viewer` has decoded every frame up to `frame_id`.
///
/// # Safety
/// `enc` must be a valid encoder.
#[no_mangle]
pub unsafe extern "C" fn gx_encoder_ack(enc: *mut GxEncoder, viewer: u32, frame_id: u32, now_ms: u64) {
    if let Some(e) = enc.as_mut() {
        e.session.ack(viewer, frame_id, now_ms);
    }
}

/// Notes a round-trip time measured for `viewer`.
///
/// # Safety
/// `enc` must be a valid encoder.
#[no_mangle]
pub unsafe extern "C" fn gx_encoder_rtt(enc: *mut GxEncoder, viewer: u32, rtt_ms: u32) {
    if let Some(e) = enc.as_mut() {
        e.session.rtt(viewer, rtt_ms);
    }
}

/// Asks for a keyframe for `viewer` (e.g. after its decoder reset).
///
/// # Safety
/// `enc` must be a valid encoder.
#[no_mangle]
pub unsafe extern "C" fn gx_encoder_request_keyframe(enc: *mut GxEncoder, viewer: u32) {
    if let Some(e) = enc.as_mut() {
        e.session.request_keyframe(viewer);
    }
}

/// Current adaptive bitrate target (kbit/s).
///
/// # Safety
/// `enc` must be a valid encoder.
#[no_mangle]
pub unsafe extern "C" fn gx_encoder_target_kbps(enc: *const GxEncoder) -> u32 {
    enc.as_ref().map_or(0, |e| e.session.target_kbps())
}

/// Number of stripes. Each stripe is an independent H.264 stream.
///
/// # Safety
/// `enc` must be a valid encoder.
#[no_mangle]
pub unsafe extern "C" fn gx_encoder_stripe_count(enc: *const GxEncoder) -> usize {
    enc.as_ref().map_or(0, |e| e.session.stripes().len())
}

/// Reads the top row and height of stripe `i` (coded pixels; the last
/// stripe may extend one row past an odd display height). Returns false if
/// there is no such stripe.
///
/// # Safety
/// `enc` must be a valid encoder; `y` and `height` must be writable.
#[no_mangle]
pub unsafe extern "C" fn gx_encoder_stripe(
    enc: *const GxEncoder,
    i: usize,
    y: *mut u32,
    height: *mut u32,
) -> bool {
    let Some(&(sy, sh)) = enc.as_ref().and_then(|e| e.session.stripes().get(i)) else {
        return false;
    };
    if y.is_null() || height.is_null() {
        return false;
    }
    y.write(sy);
    height.write(sh);
    true
}

/// Current resolution divisor: 1 (full) or 2 (half, while the link is too
/// slow; viewers scale the picture back up).
///
/// # Safety
/// `enc` must be a valid encoder.
#[no_mangle]
pub unsafe extern "C" fn gx_encoder_scale(enc: *const GxEncoder) -> u32 {
    enc.as_ref().map_or(1, |e| e.session.scale())
}

/// Whether `viewer`'s own deliveries show congestion now (acks queueing,
/// or its oldest unacknowledged frame overdue). For moving a slow viewer to
/// a separate, lower-quality encoder (simulcast).
///
/// # Safety
/// `enc` must be a valid encoder.
#[no_mangle]
pub unsafe extern "C" fn gx_encoder_viewer_congested(enc: *const GxEncoder, viewer: u32,
        now_ms: u64) -> bool {
    enc.as_ref().is_some_and(|e| e.session.viewer_congested(viewer, now_ms))
}

/// Sets where the user is working (e.g. around the mouse pointer), in
/// display pixels, encoded at better quality when `focus_boost` is set. A
/// width or height of 0 clears it.
///
/// # Safety
/// `enc` must be a valid encoder.
#[no_mangle]
pub unsafe extern "C" fn gx_encoder_set_focus(enc: *mut GxEncoder, x: i32, y: i32,
        width: i32, height: i32) {
    if let Some(e) = enc.as_mut() {
        e.session.set_focus(Some(Rect { x, y, width, height }));
    }
}
