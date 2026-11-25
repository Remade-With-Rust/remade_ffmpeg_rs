//! Inverse discrete wavelet transforms (ISO/IEC 15444-1 Annex F).
//!
//! A plane holds its subbands in nested order: the next lower resolution in
//! the top left corner, HL to its right, LH below, HH diagonal. Each call
//! merges one level in place.

use crate::error::{try_vec, JpxResult};
use crate::kernels::{synthesize_columns, synthesize_rows, COLUMN_SCRATCH};

/// Samples of a tile-component.
pub(crate) struct Plane {
    /// Integers for the 5/3, the bits of an `f32` for the 9/7.
    pub data: Vec<i32>,
    /// Which of the two.
    pub reals: bool,
}

impl Plane {
    pub fn zeroed(n: usize, reals: bool) -> JpxResult<Self> {
        Ok(Plane {
            data: try_vec(n)?,
            reals,
        })
    }

    /// Merge one decomposition level: `lo` and `hi` are the areas of the two
    /// resolutions, `stride` the width of the plane.
    pub fn synthesize(&mut self, stride: usize, lo: [u32; 4], hi: [u32; 4]) -> JpxResult<()> {
        let w = hi[2].saturating_sub(hi[0]) as usize;
        let h = hi[3].saturating_sub(hi[1]) as usize;
        let lw = lo[2].saturating_sub(lo[0]) as usize;
        let lh = lo[3].saturating_sub(lo[1]) as usize;
        if w == 0 || h == 0 {
            return Ok(());
        }
        let (odd_x, odd_y) = (hi[0] & 1 != 0, hi[1] & 1 != 0);
        let (p, reals) = (&mut self.data[..], self.reals);
        // A signal of one sample at an odd position is stored doubled.
        let halve = |v: &mut i32| {
            *v = if reals {
                (f32::from_bits(*v as u32) * 0.5).to_bits() as i32
            } else {
                *v / 2
            }
        };

        if w > 1 {
            let mut tmp = try_vec(lw)?;
            synthesize_rows(p, stride, (lw, w - lw), h, odd_x, reals, &mut tmp);
        } else if odd_x {
            p.iter_mut().step_by(stride).take(h).for_each(halve);
        }
        if h > 1 {
            let mut col = try_vec(COLUMN_SCRATCH * h)?;
            synthesize_columns(p, stride, w, (h, lh), odd_y, reals, &mut col);
        } else if odd_y {
            p[..w].iter_mut().for_each(halve);
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn plane(data: Vec<i32>) -> Plane {
        Plane { data, reals: false }
    }

    /// Direct F.3 synthesis of one signal, with explicit symmetric extension.
    fn reference53(low: &[i32], high: &[i32], odd: bool) -> Vec<i32> {
        let n = low.len() + high.len();
        let first = usize::from(odd);
        let mut y = vec![0i32; n];
        let (mut l, mut h) = (low.iter(), high.iter());
        for (i, v) in y.iter_mut().enumerate() {
            let it = if (i + first) % 2 == 0 { &mut l } else { &mut h };
            *v = *it.next().unwrap();
        }
        if n == 1 {
            return if odd { vec![y[0] / 2] } else { y };
        }
        let ext = |v: &[i32], i: isize| -> i32 {
            let p = 2 * (n as isize - 1);
            let j = i.rem_euclid(p);
            v[if j < n as isize { j } else { p - j } as usize]
        };
        let src = y.clone();
        for i in 0..n {
            if (i + first) % 2 == 0 {
                let i = i as isize;
                y[i as usize] = src[i as usize] - ((ext(&src, i - 1) + ext(&src, i + 1) + 2) >> 2);
            }
        }
        let src = y.clone();
        for i in 0..n {
            if (i + first) % 2 == 1 {
                let i = i as isize;
                y[i as usize] = src[i as usize] + ((ext(&src, i - 1) + ext(&src, i + 1)) >> 1);
            }
        }
        y
    }

    #[test]
    fn five_three_matches_direct_synthesis() {
        let mut seed = 1u32;
        for n in 1..40u32 {
            for x0 in 0..2u32 {
                let lo = x0.div_ceil(2);
                let hi = (x0 + n).div_ceil(2);
                let data: Vec<i32> = (0..n)
                    .map(|_| {
                        seed = seed.wrapping_mul(1_664_525).wrapping_add(1_013_904_223);
                        (seed >> 20) as i32 - 2048
                    })
                    .collect();
                let (low, high) = data.split_at((hi - lo) as usize);
                let want = reference53(low, high, x0 == 1);
                let row = data.clone();
                let mut row = plane(row);
                row.synthesize(n as usize, [lo, 0, hi, 1], [x0, 0, x0 + n, 1]).unwrap();
                assert_eq!(row.data, want, "row {n} {x0}");
                let col = data.clone();
                let mut col = plane(col);
                col.synthesize(1, [0, lo, 1, hi], [0, x0, 1, x0 + n]).unwrap();
                assert_eq!(col.data, want, "column {n} {x0}");
            }
        }
    }
}
