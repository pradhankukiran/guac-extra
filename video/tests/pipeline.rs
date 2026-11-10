// SPDX-License-Identifier: Apache-2.0
//! End-to-end tests of the video session: what gets encoded, when, for whom,
//! and that the output decodes (with ffmpeg, when installed).

use guac_extra_video::{Config, Rect, Session};
use std::process::Command;

const W: u32 = 320;
const H: u32 = 240;

fn config(stripes: u32) -> Config {
    Config {
        fps: 60,
        threads: 1,
        convert_threads: 1,
        stripes,
        ..Config::default()
    }
}

/// A BGRX test image: a gradient with a white box at (bx, by).
fn image(w: u32, h: u32, bx: u32, by: u32) -> Vec<u8> {
    let mut px = vec![0u8; (w * h * 4) as usize];
    for y in 0..h {
        for x in 0..w {
            let i = ((y * w + x) * 4) as usize;
            let in_box = x >= bx && x < bx + 32 && y >= by && y < by + 32;
            let v = if in_box { 255 } else { ((x + y) % 200) as u8 };
            px[i..i + 4].copy_from_slice(&[v, v, v, 255]);
        }
    }
    px
}

fn full(w: u32, h: u32) -> Rect {
    Rect { x: 0, y: 0, width: w as i32, height: h as i32 }
}

fn update(s: &mut Session, px: &[u8], w: u32, rects: &[Rect]) {
    unsafe { s.update(px.as_ptr(), (w * 4) as usize, rects) };
}

#[test]
fn new_viewer_gets_a_keyframe_then_nothing_while_still() {
    let mut s = Session::new(config(2), W, H, 0, Default::default()).unwrap();
    let v = s.add_viewer(0);
    update(&mut s, &image(W, H, 10, 10), W, &[full(W, H)]);
    assert!(s.encode(0).unwrap());
    let f = s.frame().unwrap();
    assert!(f.keyframe);
    assert_eq!(f.parts.len(), 2);
    assert_eq!(f.recipients, vec![v]);
    s.ack(v, f.id, 5);
    // Nothing changed: no frame (paint-over of a keyframe aside, below).
    assert!(!s.encode(20).unwrap());
}

#[test]
fn small_change_encodes_only_its_stripe() {
    let mut s = Session::new(config(4), W, H, 0, Default::default()).unwrap();
    let v = s.add_viewer(0);
    update(&mut s, &image(W, H, 10, 10), W, &[full(W, H)]);
    assert!(s.encode(0).unwrap());
    s.ack(v, s.frame().unwrap().id, 5);
    // Change a box in the bottom stripe only.
    update(&mut s, &image(W, H, 10, 200), W, &[Rect { x: 10, y: 200, width: 32, height: 32 }]);
    assert!(s.encode(20).unwrap());
    let f = s.frame().unwrap();
    assert!(!f.keyframe);
    assert_eq!(f.parts.len(), 1);
    assert_eq!(f.parts[0].stripe, 3);
}

#[test]
fn sharpening_skips_stripes_with_nothing_to_sharpen() {
    // Motion in the top stripe only, for 2 s, every frame acked.
    let mut s = Session::new(config(4), W, H, 0, Default::default()).unwrap();
    let v = s.add_viewer(0);
    update(&mut s, &image(W, H, 10, 10), W, &[full(W, H)]);
    let mut per_stripe = [0usize; 4];
    let mut now = 0;
    for i in 0..120u32 {
        if i > 0 {
            let x = (i * 3) % 280;
            update(&mut s, &image(W, H, x, 10), W, &[Rect { x: 0, y: 0, width: W as i32, height: 48 }]);
        }
        if s.encode(now).unwrap() {
            let f = s.frame().unwrap();
            for p in &f.parts {
                per_stripe[p.stripe as usize] += 1;
            }
            let id = f.id;
            s.ack(v, id, now + 2);
        }
        now += 17;
    }
    assert!(per_stripe[0] > 100, "{per_stripe:?}");
    // The keyframe, then one sharpening pass once still.
    assert!(per_stripe[3] <= 2, "{per_stripe:?}");
}

#[test]
fn paint_over_follows_motion() {
    let mut s = Session::new(config(1), W, H, 0, Default::default()).unwrap();
    let v = s.add_viewer(0);
    update(&mut s, &image(W, H, 10, 10), W, &[full(W, H)]);
    assert!(s.encode(0).unwrap());
    s.ack(v, s.frame().unwrap().id, 1);
    update(&mut s, &image(W, H, 50, 50), W, &[full(W, H)]);
    assert!(s.encode(20).unwrap());
    assert!(!s.frame().unwrap().paintover);
    s.ack(v, s.frame().unwrap().id, 21);
    // Too early for paint-over.
    assert!(!s.encode(100).unwrap());
    let wait = s.next_deadline_ms(100);
    assert_eq!(wait, 70, "paint-over due 150 ms after the last motion");
    assert!(s.encode(170).unwrap());
    assert!(s.frame().unwrap().paintover);
}

#[test]
fn flow_control_holds_frames_until_acked() {
    let mut s = Session::new(config(1), W, H, 0, Default::default()).unwrap();
    let v = s.add_viewer(0);
    s.rtt(v, 20);
    let mut now = 0;
    let mut sent = Vec::new();
    for i in 0..20 {
        update(&mut s, &image(W, H, (i * 8) % 280, 10), W, &[full(W, H)]);
        if s.encode(now).unwrap() {
            sent.push(s.frame().unwrap().id);
        }
        now += 17;
    }
    // 60 fps, 20 ms RTT: at most ceil(60 / 16.7) + 2 = 6 frames in flight.
    assert!(sent.len() <= 6, "sent {} frames without acks", sent.len());
    assert!(!s.encode(now).unwrap());
    s.ack(v, *sent.last().unwrap(), now);
    update(&mut s, &image(W, H, 100, 100), W, &[full(W, H)]);
    now += 17;
    assert!(s.encode(now).unwrap(), "acks release flow control");
}

#[test]
fn late_viewer_starts_on_a_keyframe() {
    let mut s = Session::new(config(1), W, H, 0, Default::default()).unwrap();
    let a = s.add_viewer(0);
    update(&mut s, &image(W, H, 10, 10), W, &[full(W, H)]);
    assert!(s.encode(0).unwrap());
    s.ack(a, s.frame().unwrap().id, 1);
    let b = s.add_viewer(10);
    update(&mut s, &image(W, H, 40, 40), W, &[Rect { x: 40, y: 40, width: 32, height: 32 }]);
    assert!(s.encode(20).unwrap());
    let f = s.frame().unwrap();
    assert!(f.keyframe, "a joining viewer forces a keyframe");
    assert_eq!(f.recipients, vec![a, b]);
}

#[test]
fn odd_display_size_works() {
    let (w, h) = (321, 241);
    let mut s = Session::new(config(3), w, h, 0, Default::default()).unwrap();
    s.add_viewer(0);
    update(&mut s, &image(w, h, 0, 0), w, &[full(w, h)]);
    assert!(s.encode(0).unwrap());
    assert!(s.frame().unwrap().keyframe);
}

#[test]
fn rejects_bad_sizes() {
    assert!(Session::new(config(1), 0, 10, 0, Default::default()).is_err());
    assert!(Session::new(config(1), 20000, 10, 0, Default::default()).is_err());
}

fn have_ffmpeg() -> bool {
    Command::new("ffmpeg").arg("-version").output().is_ok_and(|o| o.status.success())
}

/// Encodes a moving box over 30 frames, writes each stripe's stream to a
/// file, and checks ffmpeg decodes every frame without errors.
#[test]
fn output_decodes_with_ffmpeg() {
    if !have_ffmpeg() {
        eprintln!("ffmpeg not installed; skipping decode check");
        return;
    }
    let stripes = 2;
    let mut s = Session::new(config(stripes), W, H, 0, Default::default()).unwrap();
    let v = s.add_viewer(0);
    let mut streams = vec![Vec::new(); stripes as usize];
    let mut counts = vec![0usize; stripes as usize];
    let mut now = 0;
    for i in 0..30u32 {
        update(&mut s, &image(W, H, (i * 9) % 280, (i * 7) % 200), W, &[full(W, H)]);
        if s.encode(now).unwrap() {
            let f = s.frame().unwrap();
            for p in &f.parts {
                streams[p.stripe as usize].extend_from_slice(&p.data);
                counts[p.stripe as usize] += 1;
            }
            let id = f.id;
            s.ack(v, id, now + 5);
        }
        now += 17;
    }
    let dir = std::env::temp_dir().join(format!("gx-test-{}", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();
    for (i, data) in streams.iter().enumerate() {
        let path = dir.join(format!("stripe{i}.h264"));
        std::fs::write(&path, data).unwrap();
        let out = Command::new("ffprobe")
            .args(["-v", "error", "-count_frames", "-select_streams", "v:0",
                   "-show_entries", "stream=nb_read_frames,width,height", "-of", "csv=p=0"])
            .arg(&path)
            .output()
            .unwrap();
        let text = String::from_utf8_lossy(&out.stdout);
        let err = String::from_utf8_lossy(&out.stderr);
        assert!(out.status.success() && err.trim().is_empty(), "stripe {i}: {err}");
        let f: Vec<&str> = text.trim().split(',').collect();
        assert_eq!(f[0], W.to_string(), "stripe {i} width");
        assert_eq!(f[2].parse::<usize>().unwrap(), counts[i], "stripe {i} decoded frames");
    }
    std::fs::remove_dir_all(&dir).unwrap();
}

/// A link that keeps getting slower (acks arrive later and later): the
/// bitrate falls to its floor, the video drops to half resolution (and the
/// new stream decodes at half width), and comes back to full resolution
/// once acks are prompt again.
#[test]
fn halves_resolution_on_a_link_too_slow_for_the_floor() {
    let cfg = Config { scaling: true, min_kbps: 500, ..config(1) };
    let mut s = Session::new(cfg, W, H, 0, Default::default()).unwrap();
    let v = s.add_viewer(0);
    s.rtt(v, 20);
    let mut pending: Vec<(u32, u64)> = Vec::new();
    let mut now = 0u64;
    let mut switched_at = None;
    let mut small_stream = Vec::new();
    let mut first_small_part_key = None;
    for i in 0..(14_000 / 17) as u32 {
        update(&mut s, &image(W, H, (i * 5) % 280, (i * 3) % 200), W, &[full(W, H)]);
        let produced = s.encode(now).unwrap();
        // The switch happens inside encode(), which then sends the first
        // half-size frame: record from that frame on
        if switched_at.is_none() && s.scale() == 2 {
            switched_at = Some(now);
        }
        if produced {
            let f = s.frame().unwrap();
            if switched_at.is_some() {
                for p in &f.parts {
                    first_small_part_key.get_or_insert(p.keyframe);
                    small_stream.extend_from_slice(&p.data);
                }
            }
            // The queue grows: each ack takes longer than the last
            pending.push((f.id, now + 50 + now / 10));
        }
        pending.retain(|&(id, due)| {
            if due <= now {
                s.ack(v, id, now);
                false
            } else {
                true
            }
        });
        now += 17;
    }
    let at = switched_at.expect("never switched to half resolution");
    assert!(at >= 3_000, "switched after only {at} ms");
    assert!(s.target_kbps() < 500, "at half resolution the bitrate must be able to go below \
            the full-size floor, got {}", s.target_kbps());
    assert_eq!(first_small_part_key, Some(true), "half-size stream must start on a keyframe");
    if have_ffmpeg() {
        let path = std::env::temp_dir().join(format!("gx-scale-{}.h264", std::process::id()));
        std::fs::write(&path, &small_stream).unwrap();
        let out = Command::new("ffprobe")
            .args(["-v", "error", "-select_streams", "v:0", "-show_entries",
                   "stream=width,height", "-of", "csv=p=0"])
            .arg(&path)
            .output()
            .unwrap();
        std::fs::remove_file(&path).unwrap();
        assert_eq!(String::from_utf8_lossy(&out.stdout).trim(), "160,120");
    }
    // Prompt acks: full resolution again. This content is light at half
    // size, so it is the rare blind retry (after 60 s) that finds the link
    for i in 0..(70_000 / 17) as u32 {
        update(&mut s, &image(W, H, (i * 5) % 280, 10), W, &[full(W, H)]);
        if s.encode(now).unwrap() {
            let id = s.frame().unwrap().id;
            s.ack(v, id, now + 5);
        }
        now += 17;
    }
    assert_eq!(s.scale(), 1, "never returned to full resolution");
}

#[test]
fn scaling_off_by_default() {
    let mut s = Session::new(config(1), W, H, 0, Default::default()).unwrap();
    s.add_viewer(0);
    assert_eq!(s.scale(), 1);
}

#[test]
fn first_frame_id_and_viewer_congestion() {
    let cfg = Config { first_frame_id: 0x8000_0000, ..config(1) };
    let mut s = Session::new(cfg, W, H, 0, Default::default()).unwrap();
    let fast = s.add_viewer(0);
    let slow = s.add_viewer(0);
    s.rtt(fast, 20);
    s.rtt(slow, 20);
    let mut now = 0;
    for i in 0..30u32 {
        update(&mut s, &image(W, H, (i * 7) % 280, 10), W, &[full(W, H)]);
        if s.encode(now).unwrap() {
            let id = s.frame().unwrap().id;
            if i == 0 {
                assert_eq!(id, 0x8000_0000);
            }
            s.ack(fast, id, now + 2); // only the fast viewer acks
        }
        now += 17;
    }
    assert!(!s.viewer_congested(fast, now));
    assert!(s.viewer_congested(slow, now));
}

/// Colors survive the trip: solid patches of known sRGB colors, encoded and
/// decoded by ffmpeg (which follows the stream's color tags), come back
/// within a few levels. The stream is BT.601 limited range, tagged as such.
#[test]
fn colors_survive_encoding() {
    if !have_ffmpeg() {
        eprintln!("ffmpeg not installed; skipping color check");
        return;
    }
    let colors: [[u8; 3]; 8] = [
        [255, 0, 0], [0, 255, 0], [0, 0, 255], [255, 255, 255],
        [0, 0, 0], [128, 128, 128], [255, 165, 0], [70, 130, 180],
    ];
    // 8 patches of 64x64 (BGRX in memory)
    let (w, h) = (512u32, 64u32);
    let mut px = vec![0u8; (w * h * 4) as usize];
    for y in 0..h {
        for x in 0..w {
            let c = colors[(x / 64) as usize];
            let i = ((y * w + x) * 4) as usize;
            px[i..i + 4].copy_from_slice(&[c[2], c[1], c[0], 255]);
        }
    }
    let cfg = Config { crf: 10.0, ..config(1) };
    let mut s = Session::new(cfg, w, h, 0, Default::default()).unwrap();
    s.add_viewer(0);
    update(&mut s, &px, w, &[full(w, h)]);
    assert!(s.encode(0).unwrap());
    let data = s.frame().unwrap().parts[0].data.clone();
    let path = std::env::temp_dir().join(format!("gx-color-{}.h264", std::process::id()));
    std::fs::write(&path, &data).unwrap();
    let out = Command::new("ffmpeg")
        .args(["-v", "error", "-i"]).arg(&path)
        .args(["-frames:v", "1", "-f", "rawvideo", "-pix_fmt", "rgb24", "-"])
        .output()
        .unwrap();
    std::fs::remove_file(&path).unwrap();
    let rgb = out.stdout;
    assert_eq!(rgb.len(), (w * h * 3) as usize, "{}", String::from_utf8_lossy(&out.stderr));
    let mut worst = 0i32;
    for (n, c) in colors.iter().enumerate() {
        let (x, y) = (n as u32 * 64 + 32, 32u32);
        let i = ((y * w + x) * 3) as usize;
        for k in 0..3 {
            worst = worst.max((rgb[i + k] as i32 - c[k] as i32).abs());
        }
    }
    eprintln!("largest color error: {worst} levels");
    assert!(worst <= 4, "colors off by up to {worst} levels");
}
