// SPDX-License-Identifier: Apache-2.0 AND MIT
// Derived from voxl src/video.rs Refiner (MIT, Copyright (c) 2026 Tembo).
//! Regional sharpening ("paint-over").
//!
//! Blocks encoded while they changed carry motion-level quality. Once a block
//! has been still for a while it is re-encoded at paint-over quality, even
//! while other parts of the screen keep moving.

/// A band of rows [y0, y1).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Band {
    pub y0: u32,
    pub y1: u32,
}

impl Band {
    pub fn union(a: Option<Band>, b: Band) -> Band {
        match a {
            None => b,
            Some(a) => Band {
                y0: a.y0.min(b.y0),
                y1: a.y1.max(b.y1),
            },
        }
    }

    pub fn overlaps(&self, y: usize, h: usize) -> bool {
        (self.y0 as usize) < y + h && (self.y1 as usize) > y
    }
}

pub const REFINE_STILL_MS: u64 = 300;
pub const REFINE_EVERY_MS: u64 = 250;

pub struct Refiner {
    changed_ms: Vec<u64>,
    pending: Vec<bool>,
    last_refine_ms: u64,
    cols: usize,
}

impl Refiner {
    pub fn new(blocks: usize, cols: usize, now_ms: u64) -> Self {
        Refiner {
            changed_ms: vec![0; blocks],
            pending: vec![false; blocks],
            last_refine_ms: now_ms,
            cols,
        }
    }

    pub fn mark(&mut self, dirty: &[u8], now_ms: u64) {
        for (i, &d) in dirty.iter().enumerate() {
            if d != 0 {
                self.pending[i] = true;
                self.changed_ms[i] = now_ms;
            }
        }
    }

    pub fn mark_all(&mut self, now_ms: u64) {
        self.pending.fill(true);
        self.changed_ms.fill(now_ms);
    }

    pub fn clear_all(&mut self) {
        self.pending.fill(false);
    }

    fn band(r0: usize, r1: usize) -> Band {
        Band {
            y0: (r0 * 16) as u32,
            y1: (r1 * 16) as u32,
        }
    }

    /// Every block not yet sharpened (1 = encode) and the rows they span.
    pub fn pending_map(&self) -> Option<(Vec<u8>, Band)> {
        let (mut r0, mut r1) = (usize::MAX, 0);
        let map: Vec<u8> = self
            .pending
            .iter()
            .enumerate()
            .map(|(i, &p)| {
                if p {
                    r0 = r0.min(i / self.cols);
                    r1 = r1.max(i / self.cols + 1);
                }
                p as u8
            })
            .collect();
        (r1 > r0).then(|| (map, Self::band(r0, r1)))
    }

    /// Blocks due for sharpening (1 = refine) and the rows they span.
    pub fn due(&mut self, now_ms: u64) -> Option<(Vec<u8>, Band)> {
        if now_ms.saturating_sub(self.last_refine_ms) < REFINE_EVERY_MS {
            return None;
        }
        let mut map = vec![0u8; self.pending.len()];
        let (mut r0, mut r1, mut any) = (usize::MAX, 0, false);
        for (i, m) in map.iter_mut().enumerate() {
            if self.pending[i] && now_ms.saturating_sub(self.changed_ms[i]) >= REFINE_STILL_MS {
                *m = 1;
                self.pending[i] = false;
                let row = i / self.cols;
                r0 = r0.min(row);
                r1 = r1.max(row + 1);
                any = true;
            }
        }
        if !any {
            return None;
        }
        self.last_refine_ms = now_ms;
        Some((map, Self::band(r0, r1)))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn still_blocks_become_due() {
        // 2x2 blocks.
        let mut r = Refiner::new(4, 2, 0);
        r.mark(&[1, 0, 0, 0], 0);
        r.mark(&[0, 0, 0, 1], 200);
        assert!(r.due(250).is_none(), "nothing still for 300 ms yet");
        let (map, band) = r.due(400).unwrap();
        assert_eq!(map, vec![1, 0, 0, 0]);
        assert_eq!(band, Band { y0: 0, y1: 16 });
        assert!(r.due(500).is_none(), "refine at most every 250 ms");
        let (map, band) = r.due(700).unwrap();
        assert_eq!(map, vec![0, 0, 0, 1]);
        assert_eq!(band, Band { y0: 16, y1: 32 });
    }

    #[test]
    fn pending_map_covers_unsharpened() {
        let mut r = Refiner::new(4, 2, 0);
        assert!(r.pending_map().is_none());
        r.mark(&[0, 1, 0, 0], 0);
        let (map, band) = r.pending_map().unwrap();
        assert_eq!(map, vec![0, 1, 0, 0]);
        assert_eq!(band, Band { y0: 0, y1: 16 });
        r.clear_all();
        assert!(r.pending_map().is_none());
    }
}
