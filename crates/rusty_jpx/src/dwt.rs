//! Inverse discrete wavelet transforms (ISO/IEC 15444-1 Annex F).
//!
//! A plane holds its subbands in nested order: the next lower resolution in
//! the top left corner, HL to its right, LH below, HH diagonal. Each call
//! merges one level in place.

use crate::error::{try_vec, JpxResult};

const ALPHA: f32 = -1.586_134_3;
const BETA: f32 = -0.052_980_12;
const GAMMA: f32 = 0.882_911_1;
const DELTA: f32 = 0.443_506_85;
const K: f32 = 1.230_174_1;

/// Columns the vertical transform handles together.
const LANES: usize = 16;

/// Samples of a tile-component: integers for the 5/3, reals for the 9/7.
pub(crate) enum Plane {
    Int(Vec<i32>),
    Real(Vec<f32>),
}

impl Plane {
    /// Merge one decomposition level: `lo` and `hi` are the areas of the two
    /// resolutions, `stride` the width of the plane.
    pub fn synthesize(&mut self, stride: usize, lo: [u32; 4], hi: [u32; 4]) -> JpxResult<()> {
        match self {
            Plane::Int(p) => merge::<i32>(p, stride, lo, hi),
            Plane::Real(p) => merge::<f32>(p, stride, lo, hi),
        }
    }
}

/// A wavelet, by its one-dimensional synthesis.
trait Wavelet: Copy + Default {
    /// Undo the lifting of `low` and `high` (separate, contiguous); `odd` is
    /// the parity of the first output sample.
    fn lift_row(low: &mut [Self], high: &mut [Self], odd: bool);
    /// The same over columns: `col` holds the rows interleaved in output
    /// order.
    fn lift_columns(col: &mut [[Self; LANES]], odd: bool);
    /// A signal of one sample at an odd position is stored doubled.
    fn halve(self) -> Self;
}

fn merge<T: Wavelet>(p: &mut [T], stride: usize, lo: [u32; 4], hi: [u32; 4]) -> JpxResult<()> {
    let w = hi[2].saturating_sub(hi[0]) as usize;
    let h = hi[3].saturating_sub(hi[1]) as usize;
    let lw = lo[2].saturating_sub(lo[0]) as usize;
    let lh = lo[3].saturating_sub(lo[1]) as usize;
    if w == 0 || h == 0 {
        return Ok(());
    }
    let (odd_x, odd_y) = (hi[0] & 1 != 0, hi[1] & 1 != 0);

    let mut tmp: Vec<T> = try_vec(w)?;
    for row in p.chunks_mut(stride).take(h) {
        let row = &mut row[..w];
        if w == 1 {
            if odd_x {
                row[0] = row[0].halve();
            }
            continue;
        }
        let (low, high) = row.split_at_mut(lw);
        T::lift_row(low, high, odd_x);
        let (even, odd): (&[T], &[T]) = if odd_x { (high, low) } else { (low, high) };
        for (pair, (&e, &o)) in tmp.chunks_exact_mut(2).zip(even.iter().zip(odd)) {
            pair[0] = e;
            pair[1] = o;
        }
        if w & 1 != 0 {
            tmp[w - 1] = even[w / 2];
        }
        row.copy_from_slice(&tmp);
    }

    if h == 1 {
        if odd_y {
            for v in &mut p[..w] {
                *v = v.halve();
            }
        }
        return Ok(());
    }
    let mut col: Vec<[T; LANES]> = try_vec(h)?;
    // Where input row `y` goes in output order.
    let place = |y: usize| {
        if y < lh {
            2 * y + usize::from(odd_y)
        } else {
            2 * (y - lh) + usize::from(!odd_y)
        }
    };
    let mut x = 0;
    while x < w {
        let n = LANES.min(w - x);
        for (y, row) in p.chunks(stride).take(h).enumerate() {
            match row[x..].first_chunk::<LANES>() {
                Some(c) if n == LANES => col[place(y)] = *c,
                _ => col[place(y)][..n].copy_from_slice(&row[x..x + n]),
            }
        }
        T::lift_columns(&mut col, odd_y);
        for (c, row) in col.iter().zip(p.chunks_mut(stride)) {
            match row[x..].first_chunk_mut::<LANES>() {
                Some(d) if n == LANES => *d = *c,
                _ => row[x..x + n].copy_from_slice(&c[..n]),
            }
        }
        x += n;
    }
    Ok(())
}

/// `t[i] = f(t[i], s[i - 1], s[i])` over two half-signals, where `t`'s first
/// sample comes before (`lead`) or after `s`'s first; missing neighbours are
/// mirrored.
#[inline(always)]
fn lift_half<T: Copy>(t: &mut [T], s: &[T], lead: bool, f: impl Fn(T, T, T) -> T) {
    let (n, m) = (t.len(), s.len());
    if n == 0 || m == 0 {
        return;
    }
    if lead {
        // Neighbours of t[i] are s[i - 1] and s[i].
        t[0] = f(t[0], s[0], s[0]);
        let k = n.min(m);
        for ((v, &a), &b) in t[1..k].iter_mut().zip(&s[..k - 1]).zip(&s[1..k]) {
            *v = f(*v, a, b);
        }
        if n > m {
            t[m] = f(t[m], s[m - 1], s[m - 1]);
        }
    } else {
        // Neighbours of t[i] are s[i] and s[i + 1].
        let k = n.min(m - 1);
        for ((v, &a), &b) in t[..k].iter_mut().zip(&s[..k]).zip(&s[1..=k]) {
            *v = f(*v, a, b);
        }
        if n > k {
            t[k] = f(t[k], s[k], s[k]);
        }
    }
}

/// `c[i] = f(c[i], c[i - 1], c[i + 1])` for every other row from `first`.
#[inline(always)]
fn lift_rows<T: Copy>(c: &mut [[T; LANES]], first: usize, f: impl Fn(T, T, T) -> T) {
    let apply = |v: &mut [T; LANES], a: &[T; LANES], b: &[T; LANES]| {
        for ((v, &a), &b) in v.iter_mut().zip(a).zip(b) {
            *v = f(*v, a, b);
        }
    };
    if first == 0 {
        let [v, b, ..] = c else { return };
        apply(v, b, b);
    }
    let mut rest = &mut c[1 - first..];
    while let [a, v, b, ..] = rest {
        apply(v, a, b);
        rest = &mut rest[2..];
    }
    if let [a, v] = rest {
        apply(v, a, a);
    }
}

#[inline(always)]
fn update53(v: i32, a: i32, b: i32) -> i32 {
    v.wrapping_sub(a.wrapping_add(b).wrapping_add(2) >> 2)
}

#[inline(always)]
fn predict53(v: i32, a: i32, b: i32) -> i32 {
    v.wrapping_add(a.wrapping_add(b) >> 1)
}

impl Wavelet for i32 {
    fn lift_row(low: &mut [i32], high: &mut [i32], odd: bool) {
        lift_half(low, high, !odd, update53);
        lift_half(high, low, odd, predict53);
    }

    fn lift_columns(col: &mut [[i32; LANES]], odd: bool) {
        lift_rows(col, usize::from(odd), update53);
        lift_rows(col, usize::from(!odd), predict53);
    }

    fn halve(self) -> i32 {
        self / 2
    }
}

impl Wavelet for f32 {
    fn lift_row(low: &mut [f32], high: &mut [f32], odd: bool) {
        for v in low.iter_mut() {
            *v *= K;
        }
        for v in high.iter_mut() {
            *v *= 1.0 / K;
        }
        lift_half(low, high, !odd, |v, a, b| v - DELTA * (a + b));
        lift_half(high, low, odd, |v, a, b| v - GAMMA * (a + b));
        lift_half(low, high, !odd, |v, a, b| v - BETA * (a + b));
        lift_half(high, low, odd, |v, a, b| v - ALPHA * (a + b));
    }

    fn lift_columns(col: &mut [[f32; LANES]], odd: bool) {
        for (i, row) in col.iter_mut().enumerate() {
            let k = if (i & 1 != 0) == odd { K } else { 1.0 / K };
            *row = row.map(|v| v * k);
        }
        let (l, h) = (usize::from(odd), usize::from(!odd));
        lift_rows(col, l, |v, a, b| v - DELTA * (a + b));
        lift_rows(col, h, |v, a, b| v - GAMMA * (a + b));
        lift_rows(col, l, |v, a, b| v - BETA * (a + b));
        lift_rows(col, h, |v, a, b| v - ALPHA * (a + b));
    }

    fn halve(self) -> f32 {
        self * 0.5
    }
}

#[cfg(test)]
mod tests {
    use super::*;

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
                let mut row = data.clone();
                merge::<i32>(&mut row, n as usize, [lo, 0, hi, 1], [x0, 0, x0 + n, 1]).unwrap();
                assert_eq!(row, want, "row {n} {x0}");
                let mut col = data.clone();
                merge::<i32>(&mut col, 1, [0, lo, 1, hi], [0, x0, 1, x0 + n]).unwrap();
                assert_eq!(col, want, "column {n} {x0}");
            }
        }
    }
}
