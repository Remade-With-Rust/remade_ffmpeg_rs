//! [`Precision::Fast`](crate::Precision::Fast): `f32` transforms through
//! tables and the device-link grid instead of the exact `f64` pipeline.
//!
//! Every fast path is a fixed sequence of `f32` operations (no FMA, no
//! reassociation, correctly rounded `sqrt` only), so its scalar form here and
//! its AVX2 form in `simd` give the same bits, on every target. Each one is
//! checked against the exact pipeline when it is built; a transform whose
//! tables would miss the error budget keeps the exact path.

use crate::curve::Curve;

/// Entries of a fast curve table: forward tables at `i / (N - 1)`, inverse
/// ones at `(i / (N - 1))^2` (indexed by the square root of the value).
pub(crate) const FAST_TABLE: usize = 4096;

/// The largest error a fast table may have against its curve (checked at
/// three points per cell when it is built). Smooth segments sit near 1e-8.
const TABLE_ERR: f64 = 1e-6;

/// How close (in linear units) a fast linear value may come to the break of
/// a two-segment output curve before its pixel takes the exact path. The
/// curve's segments miss each other at the break by the rounding of its
/// s15Fixed16 parameters (1.1e-5 for sRGB, 2.4e-4 for Rec. 709), so the
/// exact path jumps there, and a fast linear value (within about 1e-6 of the
/// exact one) on the other side of the break would take the whole jump.
/// About 8 in a million values per channel come this close.
pub(crate) const GUARD: f32 = 4e-6;

/// The steepest output curve the fast matrix-shaper path takes (output
/// units per linear unit): the `f32` linear value carries about 1e-7 of
/// rounding, which an output curve amplifies by its slope. The sRGB and
/// Rec. 709 curves' linear toes (12.92, 4.5) pass; a pure power law (Adobe
/// RGB) is infinitely steep at 0 and keeps the exact path.
const MAX_SLOPE: f64 = 32.0;

/// `max(x, 0)` as `maxps` computes it: `x` when it is above 0, else `+0.0`
/// (so NaN and `-0.0` give `+0.0`).
#[inline(always)]
pub(crate) fn max0(x: f32) -> f32 {
    if x > 0.0 {
        x
    } else {
        0.0
    }
}

/// `min(x, 1)` as `minps` computes it: `x` when it is below 1, else `1.0`.
#[inline(always)]
pub(crate) fn min1(x: f32) -> f32 {
    if x < 1.0 {
        x
    } else {
        1.0
    }
}

/// The clamp of every fast path: `min1(max0(x))`.
#[inline(always)]
pub(crate) fn clamp(x: f32) -> f32 {
    min1(max0(x))
}

/// Cells of a fast curve table: `FAST_TABLE` samples, each cell holding
/// its first sample and the difference to the next.
pub(crate) const CELLS: usize = FAST_TABLE - 1;

/// Linear interpolation at a position in `0..=CELLS` (non-negative, never
/// NaN: every caller clamps first): with `i` the truncated position capped
/// at `CELLS - 1` and `[a, d]` its cell, `a + d * (pos - i)` (the same value
/// as `t[i] + (t[i+1] - t[i]) * (pos - i)` on the samples).
#[inline(always)]
pub(crate) fn interp(t: &[[f32; 2]; CELLS], pos: f32) -> f32 {
    let i = (pos as i32 as u32).min(CELLS as u32 - 1) as usize;
    let f = pos - i as f32;
    let [a, d] = t[i];
    a + d * f
}

/// One fast curve: a table over its smooth part and, for a two-segment
/// (function 3) curve, its linear toe evaluated directly: below the break
/// the value is `v * k` (the slope `c` forward, `1 / c` inverse). The table
/// is the power segment continued below the break, so the cell holding the
/// break interpolates one smooth function (the exact curve jumps there by
/// the rounding of its parameters, which no interpolation can follow).
///
/// (A power law in `f32` instead of the table, `exp2(p * log2(x))` with
/// degree-8 and degree-6 polynomials, measured 2.3-2.6x slower in the
/// kernel: the Horner chains put the curve's latency on the pixel's
/// critical path.)
pub(crate) struct FastCurve {
    pub(crate) table: Box<[[f32; 2]; CELLS]>,
    pub(crate) toe: Option<(f32, f32)>,
}

/// A curve's smooth part (as a function) and its toe `(break, slope)`.
type Parts<'a> = (Box<dyn Fn(f64) -> f64 + 'a>, Option<(f64, f64)>);

impl FastCurve {
    /// Forward: `None` inside for an identity curve (the value itself).
    fn forward(c: &Curve) -> Option<Option<Self>> {
        if c.is_identity() {
            return Some(None);
        }
        let n = (FAST_TABLE - 1) as f64;
        let (smooth, toe): Parts<'_> = match *c {
            Curve::Parametric {
                function: 3,
                params: [g, a, b, cc, d, ..],
            } if d > 0.0 && d < 1.0 && a * d + b > 0.0 => (
                Box::new(move |x: f64| (a * x + b).max(0.0).powf(g)),
                Some((d, cc)),
            ),
            _ => (Box::new(|x: f64| c.eval(x)), None),
        };
        let f = Self {
            table: table((0..FAST_TABLE).map(|i| smooth(i as f64 / n)))?,
            toe: toe.map(|(d, k)| (d as f32, k as f32)),
        };
        // Against the curve at three points per cell.
        for i in 0..FAST_TABLE - 1 {
            for q in [0.25, 0.5, 0.75] {
                let x = (i as f64 + q) / n;
                if (f64::from(f.forward_at(x as f32)) - c.eval(x)).abs() > TABLE_ERR {
                    return None;
                }
            }
        }
        Some(Some(f))
    }

    /// Inverse: `None` inside for an identity curve.
    fn inverse(c: &Curve) -> Option<Option<Self>> {
        if c.is_identity() {
            return Some(None);
        }
        let n = (FAST_TABLE - 1) as f64;
        let (smooth, toe): Parts<'_> = match *c {
            Curve::Parametric {
                function: 3,
                params: [g, a, b, cc, d, ..],
            } if d > 0.0
                && d < 1.0
                && a.abs() > 1e-9
                && cc.abs() > 1e-9
                && g.abs() > 1e-9
                && a * d + b > 0.0 =>
            {
                let brk = (a * d + b).powf(g);
                (
                    Box::new(move |y: f64| (y.max(0.0).powf(1.0 / g) - b) / a),
                    Some((brk, 1.0 / cc)),
                )
            }
            _ => (Box::new(|y: f64| c.eval_inverse(y)), None),
        };
        let f = Self {
            table: table((0..FAST_TABLE).map(|i| smooth((i as f64 / n).powi(2))))?,
            toe: toe.map(|(brk, k)| (brk as f32, k as f32)),
        };
        for i in 0..FAST_TABLE - 1 {
            for q in [0.25, 0.5, 0.75] {
                let s = (i as f64 + q) / n;
                let y = (s * s) as f32;
                if f.near_break(y) {
                    continue; // such pixels take the exact path
                }
                let got = clamp(f.inverse_at(y));
                if (f64::from(got) - c.eval_inverse(f64::from(y))).abs() > TABLE_ERR {
                    return None;
                }
            }
        }
        // The steepest the curve gets: the toe's slope, and over the
        // `sqrt`-spaced points above the toe.
        let mut slope = f.toe.map_or(0.0, |(_, k)| f64::from(k));
        let start = f.toe.map_or(0, |(brk, _)| {
            ((f64::from(brk).sqrt() * n) as usize + 1).min(FAST_TABLE - 2)
        });
        for i in start..FAST_TABLE - 1 {
            let (y0, y1) = ((i as f64 / n).powi(2), ((i + 1) as f64 / n).powi(2));
            let dx = f.inverse_at(y1 as f32) - f.inverse_at(y0 as f32);
            slope = slope.max(f64::from(dx) / (y1 - y0));
        }
        if slope > MAX_SLOPE {
            return None;
        }
        Some(Some(f))
    }

    /// The forward value at a clamped `x`: the scalar form of the kernel.
    #[inline(always)]
    pub(crate) fn forward_at(&self, x: f32) -> f32 {
        let t = interp(&self.table, x * (FAST_TABLE - 1) as f32);
        match self.toe {
            Some((d, k)) if x < d => x * k,
            _ => t,
        }
    }

    /// `true` when `y` is within [`GUARD`] of this inverse curve's break.
    #[inline(always)]
    pub(crate) fn near_break(&self, y: f32) -> bool {
        self.toe.is_some_and(|(brk, _)| (y - brk).abs() < GUARD)
    }

    /// The inverse value at a clamped `y` (not clamped after).
    #[inline(always)]
    pub(crate) fn inverse_at(&self, y: f32) -> f32 {
        let t = interp(&self.table, y.sqrt() * (FAST_TABLE - 1) as f32);
        match self.toe {
            Some((brk, k)) if y < brk => y * k,
            _ => t,
        }
    }
}

fn table(values: impl Iterator<Item = f64>) -> Option<Box<[[f32; 2]; CELLS]>> {
    let v: Vec<f32> = values.map(|v| v as f32).collect();
    v.windows(2)
        .map(|w| [w[0], w[1] - w[0]])
        .collect::<Vec<_>>()
        .into_boxed_slice()
        .try_into()
        .ok()
}

/// The fast matrix-shaper curves of one transform: per input channel a
/// forward curve and per output channel an inverse one (`None`: identity).
/// The 3x3 matrix and offset are the 8- and 16-bit path's.
pub(crate) struct FastShaper {
    pub(crate) fwd: [Option<FastCurve>; 3],
    pub(crate) inv: [Option<FastCurve>; 3],
}

/// [`FastShaper`] with the matrix, borrowed for one call.
pub(crate) struct FastShaperView<'a> {
    pub(crate) fwd: [Option<&'a FastCurve>; 3],
    pub(crate) inv: [Option<&'a FastCurve>; 3],
    pub(crate) m: [[f32; 3]; 3],
    pub(crate) off: [f32; 3],
}

impl FastShaper {
    /// The curves for `in_curves` and `out_curves`, or `None` when one of
    /// them misses [`TABLE_ERR`] or (output) is steeper than [`MAX_SLOPE`].
    pub(crate) fn new(in_curves: &[Curve], out_curves: &[Curve]) -> Option<Self> {
        let ([i0, i1, i2], [o0, o1, o2]) = (in_curves, out_curves) else {
            return None;
        };
        Some(Self {
            fwd: [
                FastCurve::forward(i0)?,
                FastCurve::forward(i1)?,
                FastCurve::forward(i2)?,
            ],
            inv: [
                FastCurve::inverse(o0)?,
                FastCurve::inverse(o1)?,
                FastCurve::inverse(o2)?,
            ],
        })
    }

    pub(crate) fn view(&self, m: [[f32; 3]; 3], off: [f32; 3]) -> FastShaperView<'_> {
        FastShaperView {
            fwd: [
                self.fwd[0].as_ref(),
                self.fwd[1].as_ref(),
                self.fwd[2].as_ref(),
            ],
            inv: [
                self.inv[0].as_ref(),
                self.inv[1].as_ref(),
                self.inv[2].as_ref(),
            ],
            m,
            off,
        }
    }
}

impl FastShaperView<'_> {
    /// One pixel: the scalar form of the fast matrix-shaper kernel, and
    /// whether a linear value came within [`GUARD`] of an output curve's
    /// break (the caller then takes the exact path for the pixel).
    #[inline(always)]
    pub(crate) fn pixel(&self, x: [f32; 3]) -> ([f32; 3], bool) {
        let mut lin = [0.0f32; 3];
        for c in 0..3 {
            let v = clamp(x[c]);
            lin[c] = match self.fwd[c] {
                Some(f) => f.forward_at(v),
                None => v,
            };
        }
        let mut y = [0.0f32; 3];
        let mut near = false;
        for (o, y) in y.iter_mut().enumerate() {
            let r = self.m[o];
            let v = clamp(r[0] * lin[0] + r[1] * lin[1] + r[2] * lin[2] + self.off[o]);
            *y = match self.inv[o] {
                Some(f) => {
                    near |= f.near_break(v);
                    clamp(f.inverse_at(v))
                }
                None => v,
            };
        }
        (y, near)
    }

    /// The scalar loop: interleaved RGB or RGBA `f32` in and out, alpha
    /// copied or set to 1; the pixels near an output break are pushed to
    /// `near` (as indices counted from `first`).
    pub(crate) fn run(
        &self,
        src: &[f32],
        dst: &mut [f32],
        (sc, dc): (usize, usize),
        (src_alpha, dst_alpha): (bool, bool),
        (first, near): (usize, &mut Vec<usize>),
    ) {
        let pixels = src.chunks_exact(sc).zip(dst.chunks_exact_mut(dc));
        for (k, (sp, dp)) in pixels.enumerate() {
            let (y, n) = self.pixel([sp[0], sp[1], sp[2]]);
            if n {
                near.push(first + k);
            }
            dp[..3].copy_from_slice(&y);
            if dst_alpha {
                dp[dc - 1] = if src_alpha { sp[sc - 1] } else { 1.0 };
            }
        }
    }
}
