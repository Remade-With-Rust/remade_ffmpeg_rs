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
        Self::build_forward(smooth, toe, |x| c.eval(x)).map(Some)
    }

    /// A forward curve from its smooth part and toe, checked against
    /// `exact` at three points per cell.
    fn build_forward(
        smooth: impl Fn(f64) -> f64,
        toe: Option<(f64, f64)>,
        exact: impl Fn(f64) -> f64,
    ) -> Option<Self> {
        let n = (FAST_TABLE - 1) as f64;
        let f = Self {
            table: table((0..FAST_TABLE).map(|i| smooth(i as f64 / n)))?,
            toe: toe.map(|(d, k)| (d as f32, k as f32)),
        };
        for i in 0..FAST_TABLE - 1 {
            for q in [0.25, 0.5, 0.75] {
                let x = (i as f64 + q) / n;
                if (f64::from(f.forward_at(x as f32)) - exact(x)).abs() > TABLE_ERR {
                    return None;
                }
            }
        }
        Some(f)
    }

    /// The in-line forward curve of a LUT chain (`FwdSeg`, function 3).
    pub(crate) fn forward_seg(s: &crate::curve::FwdSeg) -> Option<Self> {
        let (g, a, b) = (s.g, s.a, s.b);
        Self::build_forward(
            move |x| (a * x + b).max(0.0).powf(g),
            Some((s.d, s.c)),
            |x| s.eval(x),
        )
    }

    /// The in-line inverse curve of a LUT chain (`InvSeg`, function 3); its
    /// break is not guarded (the chains' budget is in CIEDE2000).
    pub(crate) fn inverse_seg(s: &crate::curve::InvSeg) -> Option<Self> {
        let n = (FAST_TABLE - 1) as f64;
        let (ig, a, b) = (s.ig, s.a, s.b);
        let smooth = move |y: f64| (y.max(0.0).powf(ig) - b) / a;
        let f = Self {
            table: table((0..FAST_TABLE).map(|i| smooth((i as f64 / n).powi(2))))?,
            toe: Some((s.brk as f32, (1.0 / s.c) as f32)),
        };
        for i in 0..FAST_TABLE - 1 {
            for q in [0.25, 0.5, 0.75] {
                let sq = (i as f64 + q) / n;
                let y = (sq * sq) as f32;
                if f.near_break(y) {
                    continue;
                }
                if (f64::from(clamp(f.inverse_at(y))) - s.eval(f64::from(y))).abs() > TABLE_ERR {
                    return None;
                }
            }
        }
        Some(f)
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

// ---- LUT chains: float CMYK ----

/// Lane-wise `a + b`, `a - b`, `a * f` on four `f32` lanes.
#[inline(always)]
pub(crate) fn add4(a: [f32; 4], b: [f32; 4]) -> [f32; 4] {
    [a[0] + b[0], a[1] + b[1], a[2] + b[2], a[3] + b[3]]
}

#[inline(always)]
pub(crate) fn sub4(a: [f32; 4], b: [f32; 4]) -> [f32; 4] {
    [a[0] - b[0], a[1] - b[1], a[2] - b[2], a[3] - b[3]]
}

#[inline(always)]
pub(crate) fn mul4(a: [f32; 4], f: f32) -> [f32; 4] {
    [a[0] * f, a[1] * f, a[2] * f, a[3] * f]
}

/// `a + (b - a) * f` lane-wise.
#[inline(always)]
pub(crate) fn lerp4(a: [f32; 4], b: [f32; 4], f: f32) -> [f32; 4] {
    add4(a, mul4(sub4(b, a), f))
}

/// The tetrahedron of a cell by the order of its three fractions: the axes
/// from the largest fraction to the smallest (ties: the lower axis first),
/// indexed by `(f0 >= f1) | (f1 >= f2) << 1 | (f0 >= f2) << 2`. Codes 2 and
/// 5 contradict themselves (they cannot occur) and repeat a neighbour.
pub(crate) const ORDER: [[usize; 3]; 8] = [
    [2, 1, 0], // f0 < f1, f1 < f2, f0 < f2
    [2, 0, 1], // f0 >= f1, f1 < f2, f0 < f2
    [1, 2, 0], // impossible
    [1, 2, 0], // f0 < f1, f1 >= f2, f0 < f2
    [2, 1, 0], // f0 < f1, f1 < f2, f0 >= f2: impossible
    [0, 2, 1], // f0 >= f1, f1 < f2, f0 >= f2
    [1, 0, 2], // f0 < f1, f1 >= f2, f0 >= f2
    [0, 1, 2], // f0 >= f1, f1 >= f2, f0 >= f2
];

#[inline(always)]
pub(crate) fn order(f: [f32; 3]) -> [usize; 3] {
    ORDER[usize::from(f[0] >= f[1])
        | usize::from(f[1] >= f[2]) << 1
        | usize::from(f[0] >= f[2]) << 2]
}

/// A profile CLUT in `f32` on four-lane points (a zero fourth lane for three
/// outputs), interpolated as the exact path does it: trilinear for a
/// Lab-indexed three-input CLUT, tetrahedral for other three-input ones,
/// linear along the first input over two tetrahedral lookups for four.
pub(crate) struct FastClut {
    pub(crate) nodes: Box<[[f32; 4]]>,
    pub(crate) g: [usize; 4],
    pub(crate) strides: [usize; 4],
    pub(crate) trilinear: bool,
}

impl FastClut {
    fn new(clut: &crate::lut::ClutEval) -> Option<Self> {
        let dims = clut.dims();
        let (inputs, outputs) = (dims.len(), clut.outputs);
        if !matches!(inputs, 3 | 4) || !matches!(outputs, 3 | 4) || dims.iter().any(|&d| d < 2) {
            return None;
        }
        let points = dims.iter().product::<usize>();
        let data = clut.data();
        if data.len() != points * outputs {
            return None;
        }
        let nodes = data
            .chunks_exact(outputs)
            .map(|v| core::array::from_fn(|k| v.get(k).copied().unwrap_or(0.0)))
            .collect();
        let mut g = [2usize; 4];
        let mut strides = [0usize; 4];
        let mut s = 1;
        for i in (0..inputs).rev() {
            g[i] = dims[i];
            strides[i] = s;
            s *= dims[i];
        }
        Some(Self {
            nodes,
            g,
            strides,
            trilinear: clut.trilinear && inputs == 3,
        })
    }

    /// Cell and fraction of `x` (in 0..=1) on axis `k`: the cell capped at
    /// `g - 2`, so the far corner always exists.
    #[inline(always)]
    pub(crate) fn axis(&self, k: usize, x: f32) -> (usize, f32) {
        let p = clamp(x) * (self.g[k] - 1) as f32;
        let i = (p as i32 as u32).min(self.g[k] as u32 - 2) as usize;
        (i, p - i as f32)
    }

    /// The tetrahedral value in the cell at `base` (strides `s`, fractions
    /// `f`): `((p0 + (p1 - p0) fa) + (p2 - p1) fb) + (p3 - p2) fc`.
    #[inline(always)]
    fn tetra(&self, base: usize, f: [f32; 3], s: [usize; 3]) -> [f32; 4] {
        let [a, b, c] = order(f);
        let p0 = self.nodes[base];
        let p1 = self.nodes[base + s[a]];
        let p2 = self.nodes[base + s[a] + s[b]];
        let p3 = self.nodes[base + s[0] + s[1] + s[2]];
        let v = add4(p0, mul4(sub4(p1, p0), f[a]));
        let v = add4(v, mul4(sub4(p2, p1), f[b]));
        add4(v, mul4(sub4(p3, p2), f[c]))
    }

    /// Three inputs.
    #[inline(always)]
    pub(crate) fn lookup3(&self, x: [f32; 3]) -> [f32; 4] {
        let (i0, f0) = self.axis(0, x[0]);
        let (i1, f1) = self.axis(1, x[1]);
        let (i2, f2) = self.axis(2, x[2]);
        let s = [self.strides[0], self.strides[1], self.strides[2]];
        let base = i0 * s[0] + i1 * s[1] + i2 * s[2];
        if self.trilinear {
            // Along the last input, then the middle, then the first.
            let (a, b, c) = (s[0], s[1], s[2]);
            let nd = &self.nodes;
            let l00 = lerp4(nd[base], nd[base + c], f2);
            let l01 = lerp4(nd[base + b], nd[base + b + c], f2);
            let l10 = lerp4(nd[base + a], nd[base + a + c], f2);
            let l11 = lerp4(nd[base + a + b], nd[base + a + b + c], f2);
            lerp4(lerp4(l00, l01, f1), lerp4(l10, l11, f1), f0)
        } else {
            self.tetra(base, [f0, f1, f2], s)
        }
    }

    /// Four inputs: linear along the first between two tetrahedral lookups.
    #[inline(always)]
    pub(crate) fn lookup4(&self, x: [f32; 4]) -> [f32; 4] {
        let (iw, fw) = self.axis(0, x[0]);
        let (i1, f1) = self.axis(1, x[1]);
        let (i2, f2) = self.axis(2, x[2]);
        let (i3, f3) = self.axis(3, x[3]);
        let s = [self.strides[1], self.strides[2], self.strides[3]];
        let base = iw * self.strides[0] + i1 * s[0] + i2 * s[1] + i3 * s[2];
        let lo = self.tetra(base, [f1, f2, f3], s);
        let hi = self.tetra(base + self.strides[0], [f1, f2, f3], s);
        lerp4(lo, hi, fw)
    }
}

/// A LUT input or output table (`Sampled`, 16-bit samples) in `f32`: cells
/// of `(sample, difference) / 65535`; `None` for the identity (two samples,
/// 0 and 65535) and for unit-gamma stages.
pub(crate) struct Table1 {
    pub(crate) cells: Box<[[f32; 2]]>,
    pub(crate) scale: f32,
    pub(crate) last: u32,
}

impl Table1 {
    fn new(s: &crate::curve::Sampled) -> Option<Self> {
        if s.seg.len() == 1 && s.seg[0] == [0.0, 65535.0] {
            return None;
        }
        Some(Self {
            cells: s
                .seg
                .iter()
                .map(|&[a, d]| [(a / 65535.0) as f32, (d / 65535.0) as f32])
                .collect(),
            scale: s.scale as f32,
            last: (s.seg.len() - 1) as u32,
        })
    }

    /// At `x` in 0..=1.
    #[inline(always)]
    pub(crate) fn eval(&self, x: f32) -> f32 {
        let pos = x * self.scale;
        let i = (pos as i32 as u32).min(self.last) as usize;
        let f = pos - i as f32;
        let [a, d] = self.cells[i];
        a + d * f
    }

    fn stage<const N: usize>(c: &crate::pipeline::StageCurves<'_>) -> [Option<Self>; N] {
        match c {
            crate::pipeline::StageCurves::Sampled(cs) => {
                core::array::from_fn(|k| cs.get(k).and_then(Self::new))
            }
            crate::pipeline::StageCurves::Unit => core::array::from_fn(|_| None),
        }
    }
}

/// Applies a stage of tables to the clamped values (identity lanes as they
/// are, clamped, as the exact unit-gamma stage and sampled tables do).
#[inline(always)]
pub(crate) fn apply<const N: usize>(tables: &[Option<Table1>; N], mut v: [f32; N]) -> [f32; N] {
    for (x, t) in v.iter_mut().zip(tables) {
        let c = clamp(*x);
        *x = match t {
            Some(t) => t.eval(c),
            None => c,
        };
    }
    v
}

/// The L*a*b* function `f(t)` (cube root above `(24/116)^3`, linear below)
/// for the fast path: the linear part as it is, the cube root by linear
/// interpolation in 128 buckets per octave of `t` (indexed by its bit
/// pattern) from `2^-7` to `2^2` (relative error under 2e-6).
pub(crate) struct LabF {
    pub(crate) cells: Box<[[f32; 2]; LABF_CELLS]>,
}

/// The first bucket's bit pattern: `2^-7`, below `(24/116)^3`.
pub(crate) const LABF_BASE: u32 = (127 - 7) << 23;
pub(crate) const LABF_SHIFT: u32 = 23 - 7;
pub(crate) const LABF_CELLS: usize = 9 << 7;
pub(crate) const LABF_LIMIT: f32 = (24.0 / 116.0) * (24.0 / 116.0) * (24.0 / 116.0);
/// Values at or above this take the last bucket's top (`f` is clamped
/// there: no L*a*b* input from a clamped device colour comes near it).
pub(crate) const LABF_TOP: f32 = 3.999;

impl LabF {
    fn new() -> Self {
        let at =
            |j: usize| f64::from(f32::from_bits(LABF_BASE + ((j as u32) << LABF_SHIFT))).cbrt();
        let cells: Vec<[f32; 2]> = (0..LABF_CELLS)
            .map(|j| [at(j) as f32, (at(j + 1) - at(j)) as f32])
            .collect();
        Self {
            cells: cells
                .into_boxed_slice()
                .try_into()
                .unwrap_or_else(|_| unreachable!("LABF_CELLS cells")),
        }
    }

    #[inline(always)]
    pub(crate) fn eval(&self, t: f32) -> f32 {
        if t <= LABF_LIMIT {
            return (841.0 / 108.0) * t + 16.0 / 116.0;
        }
        let t = if t < LABF_TOP { t } else { LABF_TOP };
        let off = t.to_bits() - LABF_BASE;
        let i = ((off >> LABF_SHIFT) as usize).min(LABF_CELLS - 1);
        let f = (off & ((1 << LABF_SHIFT) - 1)) as f32 * (1.0 / (1 << LABF_SHIFT) as f32);
        let [a, d] = self.cells[i];
        a + d * f
    }
}

/// `f^-1` of L*a*b* (cube above `24/116`, linear below), plain arithmetic.
#[inline(always)]
pub(crate) fn lab_f_inv(t: f32) -> f32 {
    if t <= 24.0 / 116.0 {
        (108.0 / 841.0) * (t - 16.0 / 116.0)
    } else {
        t * t * t
    }
}

/// The affine maps of a chain composed into one, in `f32`.
fn compose(affines: &[(crate::color::Mat3, [f64; 3])]) -> ([[f32; 3]; 3], [f32; 3]) {
    let mut m = [[1.0, 0.0, 0.0], [0.0, 1.0, 0.0], [0.0, 0.0, 1.0]];
    let mut off = [0.0f64; 3];
    for (a, o) in affines {
        let r = |v: [f64; 3]| crate::color::mat_vec(a, v);
        let cols: [[f64; 3]; 3] = core::array::from_fn(|j| r([m[0][j], m[1][j], m[2][j]]));
        m = core::array::from_fn(|i| core::array::from_fn(|j| cols[j][i]));
        let t = r(off);
        off = [t[0] + o[0], t[1] + o[1], t[2] + o[2]];
    }
    (m.map(|r| r.map(|v| v as f32)), off.map(|v| v as f32))
}

#[inline(always)]
fn affine(m: &[[f32; 3]; 3], off: &[f32; 3], v: [f32; 3]) -> [f32; 3] {
    [
        m[0][0] * v[0] + m[0][1] * v[1] + m[0][2] * v[2] + off[0],
        m[1][0] * v[0] + m[1][1] * v[1] + m[1][2] * v[2] + off[1],
        m[2][0] * v[0] + m[2][1] * v[1] + m[2][2] * v[2] + off[2],
    ]
}

/// RGB (matrix/TRC) into a Lab-PCS LUT output profile, fast: the forward
/// curves and the Lab cube root by tables, the affine maps composed, then
/// the profile's own input tables, CLUT and output tables in `f32`.
pub(crate) struct FastLutOut {
    pub(crate) fwd: [FastCurve; 3],
    pub(crate) m: [[f32; 3]; 3],
    pub(crate) off: [f32; 3],
    pub(crate) lab: LabF,
    pub(crate) lab_scale: f32,
    pub(crate) pre: [Option<Table1>; 3],
    pub(crate) clut: FastClut,
    pub(crate) post: [Option<Table1>; 4],
    pub(crate) outputs: usize,
}

impl FastLutOut {
    fn new<const N: usize>(c: &crate::pipeline::LutOutChain<'_, N>) -> Option<Self> {
        let fwd = [0, 1, 2].map(|k| FastCurve::forward_seg(&c.curves[k]));
        let [Some(f0), Some(f1), Some(f2)] = fwd else {
            return None;
        };
        // The PCS white (D50) folded into the map: X / Xn as one product.
        let white = crate::color::D50;
        let mut affines: Vec<_> = c.affines[..c.maps].to_vec();
        affines.push((
            [
                [1.0 / white.x, 0.0, 0.0],
                [0.0, 1.0 / white.y, 0.0],
                [0.0, 0.0, 1.0 / white.z],
            ],
            [0.0; 3],
        ));
        let (m, off) = compose(&affines);
        Some(Self {
            fwd: [f0, f1, f2],
            m,
            off,
            lab: LabF::new(),
            lab_scale: c.lab_scale.unwrap_or(1.0) as f32,
            pre: core::array::from_fn(|k| Table1::new(&c.pre[k])),
            clut: FastClut::new(c.clut.clut)?,
            post: core::array::from_fn(|k| c.post.get(k).and_then(Table1::new)),
            outputs: N,
        })
    }

    /// The encoded Lab of one RGB colour (the CLUT's input).
    #[inline(always)]
    pub(crate) fn lab(&self, x: [f32; 3]) -> [f32; 3] {
        let lin = [
            self.fwd[0].forward_at(clamp(x[0])),
            self.fwd[1].forward_at(clamp(x[1])),
            self.fwd[2].forward_at(clamp(x[2])),
        ];
        let t = affine(&self.m, &self.off, lin);
        let (fx, fy, fz) = (
            self.lab.eval(t[0]),
            self.lab.eval(t[1]),
            self.lab.eval(t[2]),
        );
        let (l, a, b) = (116.0 * fy - 16.0, 500.0 * (fx - fy), 200.0 * (fy - fz));
        let e = [
            clamp(l * 0.01),
            clamp((a + 128.0) * (1.0 / 255.0)),
            clamp((b + 128.0) * (1.0 / 255.0)),
        ];
        let k = self.lab_scale;
        apply::<3>(&self.pre, [e[0] * k, e[1] * k, e[2] * k])
    }

    /// One pixel: the scalar form of the kernel.
    #[inline(always)]
    pub(crate) fn pixel(&self, x: [f32; 3]) -> [f32; 4] {
        let v = apply::<4>(&self.post, self.clut.lookup3(self.lab(x)));
        [clamp(v[0]), clamp(v[1]), clamp(v[2]), clamp(v[3])]
    }
}

/// A Lab-PCS LUT input profile into RGB (matrix/TRC), fast: the profile's
/// input tables, CLUT and output tables in `f32`, the Lab decoding and
/// affine maps (composed) in `f32`, the inverse curves by tables.
pub(crate) struct FastLutIn {
    pub(crate) pre: [Option<Table1>; 4],
    pub(crate) clut: FastClut,
    pub(crate) post: [Option<Table1>; 3],
    pub(crate) lab_scale: f32,
    pub(crate) m: [[f32; 3]; 3],
    pub(crate) off: [f32; 3],
    pub(crate) inv: [FastCurve; 3],
}

impl FastLutIn {
    fn new(c: &crate::pipeline::LutInChain<'_>) -> Option<Self> {
        let inv = [0, 1, 2].map(|k| FastCurve::inverse_seg(&c.inverse[k]));
        let [Some(i0), Some(i1), Some(i2)] = inv else {
            return None;
        };
        let white = crate::color::D50;
        let mut affines = vec![(
            [
                [white.x, 0.0, 0.0],
                [0.0, white.y, 0.0],
                [0.0, 0.0, white.z],
            ],
            [0.0; 3],
        )];
        affines.extend_from_slice(&c.affines[..c.maps]);
        let (m, off) = compose(&affines);
        Some(Self {
            pre: Table1::stage::<4>(&c.pre),
            clut: FastClut::new(c.clut)?,
            post: Table1::stage::<3>(&c.post),
            lab_scale: c.lab_scale.unwrap_or(1.0) as f32,
            m,
            off,
            inv: [i0, i1, i2],
        })
    }

    /// The pixel after the CLUT: output tables, Lab decoding, the map, the
    /// clamp and the inverse curves.
    #[inline(always)]
    pub(crate) fn finish(&self, v: [f32; 4]) -> [f32; 3] {
        let e = apply::<3>(&self.post, [v[0], v[1], v[2]]);
        let k = self.lab_scale;
        let e = [e[0] * k, e[1] * k, e[2] * k];
        let (l, a, b) = (e[0] * 100.0, e[1] * 255.0 - 128.0, e[2] * 255.0 - 128.0);
        let fy = (l + 16.0) * (1.0 / 116.0);
        let t = [
            lab_f_inv(fy + 0.002 * a),
            lab_f_inv(fy),
            lab_f_inv(fy - 0.005 * b),
        ];
        let lin = affine(&self.m, &self.off, t);
        [
            clamp(self.inv[0].inverse_at(clamp(lin[0]))),
            clamp(self.inv[1].inverse_at(clamp(lin[1]))),
            clamp(self.inv[2].inverse_at(clamp(lin[2]))),
        ]
    }

    #[inline(always)]
    pub(crate) fn pixel4(&self, x: [f32; 4]) -> [f32; 3] {
        let e = apply::<4>(&self.pre, x);
        self.finish(self.clut.lookup4(e))
    }

    #[inline(always)]
    pub(crate) fn pixel3(&self, x: [f32; 3]) -> [f32; 3] {
        let t = |k: usize| {
            let c = clamp(x[k]);
            self.pre[k].as_ref().map_or(c, |t| t.eval(c))
        };
        self.finish(self.clut.lookup3([t(0), t(1), t(2)]))
    }
}

/// The fast path of a LUT chain (float CMYK both ways).
pub(crate) enum FastChain {
    Out(FastLutOut),
    In(FastLutIn, usize),
}

impl FastChain {
    pub(crate) fn new(chain: &crate::pipeline::Chain) -> Option<Self> {
        if let Some(c) = chain.as_lut_out::<4>() {
            return FastLutOut::new(&c).map(Self::Out);
        }
        if let Some(c) = chain.as_lut_out::<3>() {
            return FastLutOut::new(&c).map(Self::Out);
        }
        let c = chain.as_lut_in()?;
        FastLutIn::new(&c).map(|f| Self::In(f, chain.in_ch))
    }

    pub(crate) fn channels(&self) -> (usize, usize) {
        match self {
            Self::Out(f) => (3, f.outputs),
            Self::In(_, n) => (*n, 3),
        }
    }

    /// The scalar loop: `f32` pixels, alpha copied or set to 1.
    pub(crate) fn run_scalar(
        &self,
        src: &[f32],
        dst: &mut [f32],
        (sc, dc): (usize, usize),
        (src_alpha, dst_alpha): (bool, bool),
    ) {
        let pixels = src.chunks_exact(sc).zip(dst.chunks_exact_mut(dc));
        let alpha = |sp: &[f32], dp: &mut [f32]| {
            if dst_alpha {
                dp[dc - 1] = if src_alpha { sp[sc - 1] } else { 1.0 };
            }
        };
        match self {
            Self::Out(f) => {
                let n = f.outputs;
                for (sp, dp) in pixels {
                    let v = f.pixel([sp[0], sp[1], sp[2]]);
                    dp[..n].copy_from_slice(&v[..n]);
                    alpha(sp, dp);
                }
            }
            Self::In(f, 4) => {
                for (sp, dp) in pixels {
                    dp[..3].copy_from_slice(&f.pixel4([sp[0], sp[1], sp[2], sp[3]]));
                    alpha(sp, dp);
                }
            }
            Self::In(f, _) => {
                for (sp, dp) in pixels {
                    dp[..3].copy_from_slice(&f.pixel3([sp[0], sp[1], sp[2]]));
                    alpha(sp, dp);
                }
            }
        }
    }

    /// The loop: the AVX2 kernel on the leading pixels, the scalar twin on
    /// the rest (the same values either way).
    pub(crate) fn run(
        &self,
        src: &[f32],
        dst: &mut [f32],
        (sc, dc): (usize, usize),
        alpha: (bool, bool),
    ) {
        let done = crate::simd::chain_f32_fast(self, src, dst, (sc, dc), alpha);
        self.run_scalar(&src[done * sc..], &mut dst[done * dc..], (sc, dc), alpha);
    }
}
