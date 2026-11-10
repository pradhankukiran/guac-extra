// SPDX-License-Identifier: Apache-2.0 AND MIT
// Derived from voxl src/encoder.rs (MIT, Copyright (c) 2026 Tembo).
//! Thin safe wrapper around libx264, tuned for low-latency desktop streaming.

#![allow(
    non_camel_case_types,
    non_upper_case_globals,
    non_snake_case,
    dead_code,
    clippy::all
)]
mod ffi {
    include!(concat!(env!("OUT_DIR"), "/x264.rs"));
}

use std::ffi::CString;
use std::ptr;

#[derive(Clone, Debug)]
pub struct EncoderConfig {
    pub width: u32,
    pub height: u32,
    pub fps: u32,
    pub preset: String,
    pub threads: u32,
    pub crf: f32,
    /// VBV cap in kbit/s (0 = uncapped).
    pub max_kbps: u32,
}

pub struct Encoder {
    h: *mut ffi::x264_t,
    param: ffi::x264_param_t,
    pic: ffi::x264_picture_t,
    pts: i64,
}

// The x264 handle is only used from one thread at a time.
unsafe impl Send for Encoder {}

pub struct Planes<'a> {
    pub y: &'a [u8],
    pub u: &'a [u8],
    pub v: &'a [u8],
    pub stride_y: usize,
    pub stride_uv: usize,
}

/// What x264 made of one picture.
#[derive(Clone, Copy, Debug)]
pub struct Encoded {
    pub keyframe: bool,
    /// The QP x264 started the frame at, rounded (None if x264 didn't say).
    pub qp: Option<f32>,
    /// x264's average effective CRF: the frame's CRF plus what VBV added.
    pub crf_avg: f32,
}

impl Encoder {
    pub fn new(cfg: &EncoderConfig) -> Result<Self, String> {
        if cfg.width == 0 || cfg.height == 0 || cfg.width % 2 != 0 || cfg.height % 2 != 0 {
            return Err(format!("bad encoder size {}x{}", cfg.width, cfg.height));
        }
        unsafe {
            let mut param: ffi::x264_param_t = std::mem::zeroed();
            let preset = CString::new(cfg.preset.as_str()).map_err(|e| e.to_string())?;
            let tune = c"zerolatency";
            if ffi::x264_param_default_preset(&mut param, preset.as_ptr(), tune.as_ptr()) < 0 {
                return Err(format!("bad x264 preset {}", cfg.preset));
            }
            param.i_width = cfg.width as i32;
            param.i_height = cfg.height as i32;
            param.i_csp = ffi::X264_CSP_I420 as i32;
            param.i_fps_num = cfg.fps.max(1);
            param.i_fps_den = 1;
            param.i_timebase_num = 1;
            param.i_timebase_den = 1000;
            // VFR input costs a frame of encoder delay (duration estimation).
            param.b_vfr_input = 0;
            param.i_threads = cfg.threads.max(1) as i32;
            param.b_sliced_threads = (cfg.threads > 1) as i32;
            param.i_slice_count = cfg.threads.max(1) as i32;
            param.i_keyint_max = ffi::X264_KEYINT_MAX_INFINITE as i32;
            param.i_keyint_min = 1;
            param.i_scenecut_threshold = 0;
            param.b_intra_refresh = 0;
            param.i_bframe = 0;
            param.b_annexb = 1;
            param.b_repeat_headers = 1;
            param.b_aud = 0;
            param.i_log_level = ffi::X264_LOG_WARNING as i32;
            param.rc.i_rc_method = ffi::X264_RC_CRF as i32;
            param.rc.f_rf_constant = cfg.crf;
            param.rc.i_lookahead = 0;
            // ultrafast's diamond search only follows motion of a few pixels;
            // UMH finds window drags and fast scrolls for about the same CPU.
            param.analyse.i_me_method = ffi::X264_ME_UMH as i32;
            param.analyse.i_me_range = 64;
            // Per-macroblock "unchanged" hints let x264 skip static areas.
            param.analyse.b_mb_info = 1;
            param.i_sync_lookahead = 0;
            param.rc.b_mb_tree = 0;
            if cfg.max_kbps > 0 {
                param.rc.i_vbv_max_bitrate = cfg.max_kbps as i32;
                param.rc.i_vbv_buffer_size = vbv_buffer_kbit(cfg.max_kbps, cfg.fps) as i32;
            }
            // sRGB content in BT.601 limited range, matching libyuv ARGBToI420.
            param.vui.i_colorprim = 1;
            param.vui.i_transfer = 13;
            param.vui.i_colmatrix = 6;
            param.vui.b_fullrange = 0;
            let profile = c"high";
            ffi::x264_param_apply_profile(&mut param, profile.as_ptr());

            let h = ffi::x264_encoder_open(&mut param);
            if h.is_null() {
                return Err("x264_encoder_open failed".into());
            }
            let mut pic: ffi::x264_picture_t = std::mem::zeroed();
            ffi::x264_picture_init(&mut pic);
            pic.img.i_csp = ffi::X264_CSP_I420 as i32;
            pic.img.i_plane = 3;
            Ok(Encoder {
                h,
                param,
                pic,
                pts: 0,
            })
        }
    }

    /// Reconfigures rate control (VBV buffer of ~1.25 frames at `max_kbps`).
    pub fn set_rate(&mut self, crf: f32, max_kbps: u32) {
        let buf = vbv_buffer_kbit(max_kbps, self.param.i_fps_num);
        self.set_rate_buf(crf, max_kbps, buf);
    }

    /// Reconfigures rate control with an explicit VBV buffer (kbit).
    pub fn set_rate_buf(&mut self, crf: f32, max_kbps: u32, buf_kbit: u32) {
        unsafe {
            self.param.rc.f_rf_constant = crf;
            if max_kbps > 0 {
                self.param.rc.i_vbv_max_bitrate = max_kbps as i32;
                self.param.rc.i_vbv_buffer_size = buf_kbit.max(1) as i32;
            }
            ffi::x264_encoder_reconfig(self.h, &mut self.param);
        }
    }

    /// Encodes one picture and appends the Annex-B output to `out`.
    /// `mb_info` holds one byte per macroblock, 1 (X264_MBINFO_CONSTANT) for
    /// unchanged ones; `quant_offsets` one QP offset per macroblock
    /// (negative: better quality). x264 reads both before this returns.
    pub fn encode(
        &mut self,
        p: &Planes,
        ts_ms: i64,
        force_idr: bool,
        mb_info: Option<&mut [u8]>,
        quant_offsets: Option<&mut [f32]>,
        out: &mut Vec<u8>,
    ) -> Result<Encoded, String> {
        unsafe {
            self.pic.img.plane[0] = p.y.as_ptr() as *mut u8;
            self.pic.img.plane[1] = p.u.as_ptr() as *mut u8;
            self.pic.img.plane[2] = p.v.as_ptr() as *mut u8;
            self.pic.img.i_stride[0] = p.stride_y as i32;
            self.pic.img.i_stride[1] = p.stride_uv as i32;
            self.pic.img.i_stride[2] = p.stride_uv as i32;
            self.pic.i_type = if force_idr {
                ffi::X264_TYPE_IDR as i32
            } else {
                ffi::X264_TYPE_AUTO as i32
            };
            self.pic.i_qpplus1 = 0;
            self.pic.prop.mb_info = mb_info.map(|m| m.as_mut_ptr()).unwrap_or(ptr::null_mut());
            self.pic.prop.mb_info_free = None;
            self.pic.prop.quant_offsets = quant_offsets
                .map(|q| q.as_mut_ptr())
                .unwrap_or(ptr::null_mut());
            self.pic.prop.quant_offsets_free = None;
            // Strictly increasing pts even if the clock stalls.
            self.pts = ts_ms.max(self.pts + 1);
            self.pic.i_pts = self.pts;
            let mut nals: *mut ffi::x264_nal_t = ptr::null_mut();
            let mut n: i32 = 0;
            let mut pic_out: ffi::x264_picture_t = std::mem::zeroed();
            let size =
                ffi::x264_encoder_encode(self.h, &mut nals, &mut n, &mut self.pic, &mut pic_out);
            // x264 is done with the hint map; don't leave a dangling pointer.
            self.pic.prop.mb_info = ptr::null_mut();
            self.pic.prop.quant_offsets = ptr::null_mut();
            if size < 0 {
                return Err("x264_encoder_encode failed".into());
            }
            if size > 0 && !nals.is_null() {
                // NAL payloads are contiguous in memory.
                let first = &*nals;
                out.extend_from_slice(std::slice::from_raw_parts(first.p_payload, size as usize));
            }
            Ok(Encoded {
                keyframe: pic_out.b_keyframe != 0,
                qp: (pic_out.i_qpplus1 > 0).then(|| (pic_out.i_qpplus1 - 1) as f32),
                crf_avg: pic_out.prop.f_crf_avg as f32,
            })
        }
    }
}

/// A VBV buffer of 1.25 frames at `max_kbps`, rounded up. x264 treats a
/// buffer under 1.1 frames as single-frame VBV, which MotionQuality relies
/// on it not doing.
pub fn vbv_buffer_kbit(max_kbps: u32, fps: u32) -> u32 {
    (max_kbps * 5).div_ceil(4 * fps.max(1))
}

impl Drop for Encoder {
    fn drop(&mut self) {
        unsafe { ffi::x264_encoder_close(self.h) }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn cfg() -> EncoderConfig {
        EncoderConfig {
            width: 64,
            height: 32,
            fps: 30,
            preset: "ultrafast".into(),
            threads: 1,
            crf: 24.0,
            max_kbps: 4000,
        }
    }

    #[test]
    fn first_forced_frame_is_a_keyframe_with_headers() {
        let mut e = Encoder::new(&cfg()).unwrap();
        let (y, u, v) = (vec![80u8; 64 * 32], vec![128u8; 32 * 16], vec![128u8; 32 * 16]);
        let p = Planes { y: &y, u: &u, v: &v, stride_y: 64, stride_uv: 32 };
        let mut out = Vec::new();
        let r = e.encode(&p, 0, true, None, None, &mut out).unwrap();
        assert!(r.keyframe);
        // Annex-B start code, then an SPS (NAL type 7) because headers repeat.
        assert_eq!(&out[..4], &[0, 0, 0, 1]);
        assert_eq!(out[4] & 0x1f, 7);
    }

    #[test]
    fn rejects_odd_size() {
        let mut c = cfg();
        c.width = 63;
        assert!(Encoder::new(&c).is_err());
    }

    #[test]
    fn vbv_buffer_is_over_one_frame() {
        // 30 Mbit/s at 60 fps: 500 kbit per frame, 625 for 1.25 frames.
        assert_eq!(vbv_buffer_kbit(30_000, 60), 625);
    }
}
