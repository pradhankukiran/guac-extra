// SPDX-License-Identifier: Apache-2.0 AND MIT
// Derived from voxl src/convert.rs (MIT, Copyright (c) 2026 Tembo).
//! BGRX to I420 conversion with libyuv, split across threads by row bands.

extern "C" {
    fn ARGBToI420(
        src_argb: *const u8,
        src_stride_argb: i32,
        dst_y: *mut u8,
        dst_stride_y: i32,
        dst_u: *mut u8,
        dst_stride_u: i32,
        dst_v: *mut u8,
        dst_stride_v: i32,
        width: i32,
        height: i32,
    ) -> i32;

    fn I420Scale(
        src_y: *const u8,
        src_stride_y: i32,
        src_u: *const u8,
        src_stride_u: i32,
        src_v: *const u8,
        src_stride_v: i32,
        src_width: i32,
        src_height: i32,
        dst_y: *mut u8,
        dst_stride_y: i32,
        dst_u: *mut u8,
        dst_stride_u: i32,
        dst_v: *mut u8,
        dst_stride_v: i32,
        dst_width: i32,
        dst_height: i32,
        filtering: i32,
    ) -> i32;
}

/// libyuv's kFilterBox: averages every source pixel; best for shrinking text.
const FILTER_BOX: i32 = 3;

/// Scales all of `src` into all of `dst` (any sizes).
pub fn scale(src: &I420, dst: &mut I420) {
    let (sw, dw) = (src.stride_uv(), dst.stride_uv());
    unsafe {
        I420Scale(
            src.y.as_ptr(),
            src.width as i32,
            src.u.as_ptr(),
            sw as i32,
            src.v.as_ptr(),
            sw as i32,
            src.width as i32,
            src.height as i32,
            dst.y.as_mut_ptr(),
            dst.width as i32,
            dst.u.as_mut_ptr(),
            dw as i32,
            dst.v.as_mut_ptr(),
            dw as i32,
            dst.width as i32,
            dst.height as i32,
            FILTER_BOX,
        );
    }
}

/// An I420 picture. `width` and `height` are even (the coded size); the
/// source may be one pixel narrower or shorter, and the padding stays black.
pub struct I420 {
    pub width: usize,
    pub height: usize,
    pub y: Vec<u8>,
    pub u: Vec<u8>,
    pub v: Vec<u8>,
}

impl I420 {
    pub fn new(width: usize, height: usize) -> Self {
        debug_assert!(width % 2 == 0 && height % 2 == 0);
        let (cw, ch) = (width / 2, height / 2);
        I420 {
            width,
            height,
            y: vec![16; width * height],
            u: vec![128; cw * ch],
            v: vec![128; cw * ch],
        }
    }

    pub fn stride_uv(&self) -> usize {
        self.width / 2
    }
}

#[derive(Clone, Copy)]
struct SendPtr(*mut u8);
unsafe impl Send for SendPtr {}
unsafe impl Sync for SendPtr {}
impl SendPtr {
    fn get(self) -> *mut u8 {
        self.0
    }
}

/// Runs `run(a, b)` over row chunks of [y0, y1) on up to `threads` threads;
/// chunk edges stay even for 4:2:0.
fn split_rows(y0: usize, y1: usize, threads: usize, run: impl Fn(usize, usize) + Sync) {
    let rows = y1 - y0;
    if rows == 0 {
        return;
    }
    let n = threads.clamp(1, (rows / 64).max(1));
    if n == 1 {
        run(y0, y1);
        return;
    }
    let chunk = (rows.div_ceil(n) + 1) & !1;
    std::thread::scope(|s| {
        let mut a = y0;
        while a < y1 {
            let b = (a + chunk).min(y1);
            let run = &run;
            if b == y1 {
                run(a, b);
            } else {
                s.spawn(move || run(a, b));
            }
            a = b;
        }
    });
}

/// Converts rows [y0, y1) of a BGRX image `src_w` pixels wide into `dst`.
/// `y0` must be even. Rows past the source height are left as they are.
///
/// # Safety
/// `src` must point to at least `src_h` rows of `src_stride` bytes, each
/// holding `src_w` 32-bit pixels.
pub unsafe fn convert_rows(
    src: *const u8,
    src_stride: usize,
    src_w: usize,
    src_h: usize,
    dst: &mut I420,
    y0: usize,
    y1: usize,
    threads: usize,
) {
    debug_assert!(y0 % 2 == 0);
    let y1 = y1.min(src_h);
    if y1 <= y0 {
        return;
    }
    let w = dst.width;
    let suv = dst.stride_uv();
    let cols = src_w.min(w);
    let (yp, up, vp) = (
        SendPtr(dst.y.as_mut_ptr()),
        SendPtr(dst.u.as_mut_ptr()),
        SendPtr(dst.v.as_mut_ptr()),
    );
    let src_p = SendPtr(src as *mut u8);
    split_rows(y0, y1, threads, |a, b| unsafe {
        ARGBToI420(
            src_p.get().add(a * src_stride),
            src_stride as i32,
            yp.get().add(a * w),
            w as i32,
            up.get().add((a / 2) * suv),
            suv as i32,
            vp.get().add((a / 2) * suv),
            suv as i32,
            cols as i32,
            (b - a) as i32,
        );
    });
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn white_and_black_map_to_limited_range() {
        let (w, h) = (4usize, 4usize);
        let mut src = vec![0u8; w * h * 4];
        // Top two rows white, bottom two black.
        for px in src[..w * 2 * 4].chunks_exact_mut(4) {
            px.copy_from_slice(&[255, 255, 255, 255]);
        }
        let mut dst = I420::new(w, h);
        unsafe { convert_rows(src.as_ptr(), w * 4, w, h, &mut dst, 0, h, 1) };
        assert!(dst.y[0] >= 234, "white luma {}", dst.y[0]);
        assert!(dst.y[w * 3] <= 17, "black luma {}", dst.y[w * 3]);
        assert!((dst.u[0] as i32 - 128).abs() <= 2);
    }

    #[test]
    fn scaling_halves_and_averages() {
        let mut src = I420::new(4, 4);
        // Left half white, right half black.
        for row in 0..4 {
            src.y[row * 4..row * 4 + 2].fill(235);
            src.y[row * 4 + 2..row * 4 + 4].fill(16);
        }
        let mut dst = I420::new(2, 2);
        scale(&src, &mut dst);
        assert_eq!(dst.y, vec![235, 16, 235, 16]);
    }

    #[test]
    fn odd_source_leaves_padding_black() {
        let (sw, sh) = (3usize, 3usize);
        let src = vec![255u8; sw * sh * 4];
        let mut dst = I420::new(4, 4);
        unsafe { convert_rows(src.as_ptr(), sw * 4, sw, sh, &mut dst, 0, 4, 1) };
        assert!(dst.y[0] >= 234);
        assert_eq!(dst.y[3], 16, "padding column");
        assert_eq!(dst.y[4 * 3], 16, "padding row");
    }
}
