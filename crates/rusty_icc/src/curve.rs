//! One-dimensional tone curves: `curvType` (identity, gamma, sampled) and
//! `parametricCurveType` (ICC functions 0-4), with forward and inverse
//! evaluation.

use crate::bytes::{Reader, Writer};
use crate::color::q15f16;
use crate::error::{Error, Result};

/// A tone curve as stored in a profile.
#[derive(Debug, Clone, PartialEq)]
pub enum Curve {
    /// `curvType`. No entries is the identity, one entry is a gamma exponent in
    /// `u8Fixed8Number`, two or more entries are samples spaced evenly over
    /// `0..=1`, each `0..=65535` for `0.0..=1.0`.
    Table(Vec<u16>),
    /// `parametricCurveType` function `0..=4` (ICC.1:2022 table 68) with its
    /// parameters `[g, a, b, c, d, e, f]`; parameters a function does not use
    /// are zero.
    Parametric {
        /// The function type, `0..=4`.
        function: u16,
        /// `[g, a, b, c, d, e, f]`.
        params: [f64; 7],
    },
}

const PARAM_COUNT: [usize; 5] = [1, 3, 4, 5, 7];

impl Curve {
    /// The identity curve.
    #[must_use]
    pub fn identity() -> Self {
        Self::Table(Vec::new())
    }

    /// A pure power law `y = x^gamma` (parametric function 0; the exponent is
    /// rounded to `s15Fixed16Number`).
    #[must_use]
    pub fn gamma(gamma: f64) -> Self {
        Self::Parametric {
            function: 0,
            params: [q15f16(gamma), 0.0, 0.0, 0.0, 0.0, 0.0, 0.0],
        }
    }

    /// The sRGB transfer function (IEC 61966-2-1), parametric function 3.
    #[must_use]
    pub fn srgb() -> Self {
        Self::parametric(3, &[2.4, 1.0 / 1.055, 0.055 / 1.055, 1.0 / 12.92, 0.04045])
            .unwrap_or_else(|_| Self::identity())
    }

    /// The ITU-R BT.709 / BT.2020 transfer function, parametric function 3.
    #[must_use]
    pub fn rec709() -> Self {
        Self::parametric(
            3,
            &[1.0 / 0.45, 1.0 / 1.099, 0.099 / 1.099, 1.0 / 4.5, 0.081],
        )
        .unwrap_or_else(|_| Self::identity())
    }

    /// A parametric curve. `params` must hold exactly the parameter count of
    /// `function` (1, 3, 4, 5 or 7); values are rounded to `s15Fixed16Number`.
    ///
    /// # Errors
    /// [`Error::InvalidTransform`] for an unknown function or wrong count.
    pub fn parametric(function: u16, params: &[f64]) -> Result<Self> {
        let n = *PARAM_COUNT
            .get(usize::from(function))
            .ok_or(Error::InvalidTransform("parametric function must be 0..=4"))?;
        if params.len() != n {
            return Err(Error::InvalidTransform("wrong parametric parameter count"));
        }
        let mut p = [0.0; 7];
        for (d, s) in p.iter_mut().zip(params) {
            *d = q15f16(*s);
        }
        Ok(Self::Parametric {
            function,
            params: p,
        })
    }

    /// `true` when the curve maps every input to itself.
    #[must_use]
    pub fn is_identity(&self) -> bool {
        match self {
            Self::Table(t) => t.is_empty() || (t.len() == 1 && t[0] == 256),
            Self::Parametric { function, params } => {
                *function == 0 && params[0] == 1.0
                    || *function == 1 && params[0] == 1.0 && params[1] == 1.0 && params[2] == 0.0
            }
        }
    }

    /// `true` for an identity written as a power of exactly 1 (`para`
    /// function 0 or 1, or a one-entry `curv`): [`Curve::eval`] of such a
    /// curve is `x` clamped to `0..=1`, with `0.0` for everything that is
    /// not above zero.
    pub(crate) fn is_unit_gamma(&self) -> bool {
        match self {
            Self::Table(t) => t.len() == 1 && t[0] == 256,
            Self::Parametric { .. } => self.is_identity(),
        }
    }

    /// Evaluates the curve at `x` (clamped to `0..=1`); the result is clamped
    /// to `0..=1`.
    #[must_use]
    pub fn eval(&self, x: f64) -> f64 {
        let x = clamp01(x);
        let y = match self {
            Self::Table(t) => eval_table(t, x),
            Self::Parametric { function, params } => eval_param(*function, params, x),
        };
        clamp01(y)
    }

    /// Evaluates the inverse curve at `y` (clamped to `0..=1`). Sampled
    /// curves are inverted by search over their samples (monotone ascending
    /// or descending tables; flat runs resolve to their first sample);
    /// parametric curves use the analytic inverse.
    #[must_use]
    pub fn eval_inverse(&self, y: f64) -> f64 {
        let y = clamp01(y);
        let x = match self {
            Self::Table(t) => inverse_table(t, y),
            Self::Parametric { function, params } => inverse_param(*function, params, y, None),
        };
        clamp01(x)
    }

    /// `true` when `other` is this curve bit for bit, so both give the same
    /// value everywhere (`==` would also equate `0.0` with `-0.0`
    /// parameters, which can differ in the sign of a zero result).
    pub(crate) fn same_as(&self, other: &Self) -> bool {
        match (self, other) {
            (Self::Table(a), Self::Table(b)) => a == b,
            (
                Self::Parametric {
                    function: fa,
                    params: pa,
                },
                Self::Parametric {
                    function: fb,
                    params: pb,
                },
            ) => fa == fb && pa.iter().zip(pb).all(|(x, y)| x.to_bits() == y.to_bits()),
            _ => false,
        }
    }

    pub(crate) fn parse(r: &mut Reader<'_>) -> Result<Self> {
        let sig = r.sig("curve type")?;
        r.skip(4, "curve reserved")?;
        match &sig {
            b"curv" => {
                let n = r.u32("curv count")? as usize;
                Ok(Self::Table(r.u16_vec(n, "curv entries")?))
            }
            b"para" => {
                let function = r.u16("para function")?;
                r.skip(2, "para reserved")?;
                let n = *PARAM_COUNT
                    .get(usize::from(function))
                    .ok_or(Error::Malformed("para function"))?;
                let mut params = [0.0; 7];
                for p in params.iter_mut().take(n) {
                    *p = r.s15f16("para parameters")?;
                }
                Ok(Self::Parametric { function, params })
            }
            _ => Err(Error::Malformed("curve is neither curv nor para")),
        }
    }

    pub(crate) fn encode(&self, w: &mut Writer) {
        match self {
            Self::Table(t) => {
                w.sig(*b"curv");
                w.u32(0);
                w.u32(t.len() as u32);
                for &v in t {
                    w.u16(v);
                }
            }
            Self::Parametric { function, params } => {
                w.sig(*b"para");
                w.u32(0);
                w.u16(*function);
                w.u16(0);
                let n = PARAM_COUNT
                    .get(usize::from(*function))
                    .copied()
                    .unwrap_or(1);
                for &p in params.iter().take(n) {
                    w.s15f16(p);
                }
            }
        }
    }
}

/// A two-segment parametric curve (functions 3 and 4, the shape of sRGB
/// and Rec. 709) prepared for repeated inverse evaluation: its break value
/// is worked out once instead of with a libm `pow` per value.
#[derive(Debug, Clone)]
pub(crate) struct InvCurve {
    function: u16,
    params: [f64; 7],
    brk: f64,
}

impl InvCurve {
    /// `None` for every other curve kind (their inverse has no per-curve
    /// constant worth keeping; use [`Curve::eval_inverse`]).
    pub(crate) fn new(curve: &Curve) -> Option<Self> {
        match curve {
            Curve::Parametric { function, params } if matches!(function, 3 | 4) => Some(Self {
                function: *function,
                params: *params,
                brk: inverse_break(*function, params),
            }),
            _ => None,
        }
    }

    /// Exactly [`Curve::eval_inverse`] of the curve this was made from.
    pub(crate) fn eval(&self, y: f64) -> f64 {
        let y = clamp01(y);
        clamp01(inverse_param(
            self.function,
            &self.params,
            y,
            Some(self.brk),
        ))
    }
}

/// A two-segment parametric curve of function 3 (sRGB, Rec. 709, Display
/// P3) for evaluation in line: what [`Curve::eval`] computes for it, with
/// the tests that depend on the curve alone already made and its
/// parameters at hand.
#[derive(Debug, Clone, Copy)]
pub(crate) struct FwdSeg {
    g: f64,
    a: f64,
    b: f64,
    c: f64,
    d: f64,
}

impl FwdSeg {
    /// `None` for every other curve, and for an exponent of exactly 1
    /// (which [`Curve::eval`] does not send through `pow`).
    pub(crate) fn new(curve: &Curve) -> Option<Self> {
        match curve {
            Curve::Parametric {
                function: 3,
                params: [g, a, b, c, d, ..],
            } if *g != 1.0 => Some(Self {
                g: *g,
                a: *a,
                b: *b,
                c: *c,
                d: *d,
            }),
            _ => None,
        }
    }

    /// Exactly [`Curve::eval`] of the curve this was made from.
    #[inline(always)]
    pub(crate) fn eval(&self, x: f64) -> f64 {
        let x = clamp01(x);
        let y = if x >= self.d {
            let base = self.a * x + self.b;
            if base > 0.0 {
                base.powf(self.g)
            } else {
                0.0
            }
        } else {
            self.c * x
        };
        clamp01(y)
    }
}

/// The inverse of a function-3 curve for evaluation in line: what
/// [`InvCurve::eval`] computes for it, with the tests that depend on the
/// curve alone already made and the reciprocal exponent worked out once
/// (the same division, so the same value) instead of per evaluation.
#[derive(Debug, Clone, Copy)]
pub(crate) struct InvSeg {
    ig: f64,
    a: f64,
    b: f64,
    c: f64,
    brk: f64,
}

impl InvSeg {
    /// `None` unless the curve is of function 3 and takes none of the
    /// degenerate arms of the inverse (a zero exponent, gain or slope) and
    /// its reciprocal exponent is not exactly 1.
    pub(crate) fn new(inv: &InvCurve) -> Option<Self> {
        let [g, a, b, c, ..] = inv.params;
        if inv.function != 3 || g.abs() < 1e-9 || a.abs() < 1e-9 || c.abs() < 1e-9 {
            return None;
        }
        let ig = 1.0 / g;
        if ig == 1.0 {
            return None;
        }
        Some(Self {
            ig,
            a,
            b,
            c,
            brk: inv.brk,
        })
    }

    /// Exactly [`InvCurve::eval`] of the curve this was made from.
    #[inline(always)]
    pub(crate) fn eval(&self, y: f64) -> f64 {
        let y = clamp01(y);
        let x = if y >= self.brk {
            let p = if y > 0.0 { y.powf(self.ig) } else { 0.0 };
            (p - self.b) / self.a
        } else {
            y / self.c
        };
        clamp01(x)
    }
}

/// A sampled curve (`curvType` with two or more entries, every LUT input and
/// output table) prepared for evaluation in line: what [`Curve::eval`]
/// computes for it, without the call, the tests on the curve kind and the
/// table length, the integer-to-float conversions of the samples and of the
/// table length per value, and the final clamp, which cannot change a value
/// interpolated between two samples of `0..=65535` (see [`Sampled::eval`]).
#[derive(Debug, Clone)]
pub(crate) struct Sampled {
    /// Per segment `i`: sample `i` and the difference to sample `i + 1`,
    /// both exact in `f64` (the samples are `u16`).
    seg: Box<[[f64; 2]]>,
    /// Entries minus one, as `f64`.
    scale: f64,
}

impl Sampled {
    /// `None` for every curve that is not a table of at least two entries.
    pub(crate) fn new(curve: &Curve) -> Option<Self> {
        let Curve::Table(t) = curve else {
            return None;
        };
        if t.len() < 2 {
            return None;
        }
        Some(Self {
            seg: t
                .windows(2)
                .map(|w| [f64::from(w[0]), f64::from(w[1]) - f64::from(w[0])])
                .collect(),
            scale: (t.len() - 1) as f64,
        })
    }

    /// Exactly [`Curve::eval`] of the table this was made from.
    ///
    /// The index is the same (`seg.len() - 1 == n - 2`), `b - a` is exact,
    /// and the clamp of the result is dropped because it never acts: with
    /// `0 <= f <= 1` the rounded product `(b - a) * f` lies between `0` and
    /// `b - a` (rounding is monotone), so the rounded sum lies between `a`
    /// and `b`, inside `0..=65535`, and the quotient by 65535 inside `0..=1`
    /// (never NaN: the clamped `x` is finite).
    #[inline(always)]
    pub(crate) fn eval(&self, x: f64) -> f64 {
        let pos = clamp01(x) * self.scale;
        let i = (pos as i64 as usize).min(self.seg.len() - 1);
        let f = pos - i as f64;
        let [a, d] = self.seg[i];
        (a + d * f) / 65535.0
    }
}

pub(crate) fn clamp01(v: f64) -> f64 {
    if v >= 0.0 {
        v.min(1.0)
    } else {
        // Also maps NaN to 0.
        0.0
    }
}

fn pow_pos(base: f64, e: f64) -> f64 {
    if base > 0.0 {
        // Identity curves (`para` gamma 1.0 is how v4 profiles, including
        // the Lab identity, spell "no curve") would otherwise pay a libm
        // `pow` per channel per colour; `pow(x, 1.0) == x` exactly.
        if e == 1.0 {
            base
        } else {
            base.powf(e)
        }
    } else {
        0.0
    }
}

fn eval_table(t: &[u16], x: f64) -> f64 {
    match t.len() {
        0 => x,
        1 => pow_pos(x, f64::from(t[0]) / 256.0),
        n => {
            let pos = x * (n - 1) as f64;
            // `x` is clamped to `0..=1` by the caller, so `pos >= 0` and the
            // cast truncation is the floor (no libm call). `pos` is also at
            // most `n - 1`, far below `i64::MAX` for any table that fits in
            // memory, so the signed conversion (one `cvttsd2si`) gives the
            // same index as the saturating `as usize` (a dozen instructions
            // on x86-64).
            let i = (pos as i64 as usize).min(n - 2);
            let f = pos - i as f64;
            let a = f64::from(t[i]);
            let b = f64::from(t[i + 1]);
            (a + (b - a) * f) / 65535.0
        }
    }
}

fn inverse_table(t: &[u16], y: f64) -> f64 {
    match t.len() {
        0 => y,
        1 => {
            let g = f64::from(t[0]) / 256.0;
            if g == 0.0 {
                0.0
            } else {
                pow_pos(y, 1.0 / g)
            }
        }
        n => {
            let v = y * 65535.0;
            let ascending = t[n - 1] >= t[0];
            let at = |i: usize| -> f64 {
                if ascending {
                    f64::from(t[i])
                } else {
                    f64::from(t[n - 1 - i])
                }
            };
            // First index whose sample is >= v (samples treated as
            // non-decreasing; small non-monotone wiggles resolve to the
            // first crossing).
            if v <= at(0) {
                let x = 0.0;
                return if ascending { x } else { 1.0 - x };
            }
            if v >= at(n - 1) {
                let x = 1.0;
                return if ascending { x } else { 1.0 - x };
            }
            let (mut lo, mut hi) = (0usize, n - 1);
            while hi - lo > 1 {
                let mid = (lo + hi) / 2;
                if at(mid) < v {
                    lo = mid;
                } else {
                    hi = mid;
                }
            }
            let a = at(lo);
            let b = at(hi);
            let f = if b > a { (v - a) / (b - a) } else { 0.0 };
            let x = (lo as f64 + f) / (n - 1) as f64;
            if ascending {
                x
            } else {
                1.0 - x
            }
        }
    }
}

fn eval_param(function: u16, p: &[f64; 7], x: f64) -> f64 {
    let [g, a, b, c, d, e, f] = *p;
    match function {
        0 => pow_pos(x, g),
        1 | 2 => {
            let off = if function == 2 { c } else { 0.0 };
            if a.abs() < 1e-9 {
                return off;
            }
            if x >= -b / a {
                pow_pos(a * x + b, g) + off
            } else {
                off
            }
        }
        3 => {
            if x >= d {
                pow_pos(a * x + b, g)
            } else {
                c * x
            }
        }
        4 => {
            if x >= d {
                pow_pos(a * x + b, g) + e
            } else {
                c * x + f
            }
        }
        _ => x,
    }
}

/// The output value at which the inverse of a two-segment parametric curve
/// (functions 3 and 4) switches segment: the curve's value at its break
/// point `d`. It depends on the curve alone.
fn inverse_break(function: u16, p: &[f64; 7]) -> f64 {
    let [g, a, b, _, d, e, _] = *p;
    if function == 4 {
        pow_pos(a * d + b, g) + e
    } else {
        pow_pos(a * d + b, g)
    }
}

/// `brk` is [`inverse_break`] when the caller has it already
/// ([`InvCurve`]); `None` computes it here, one libm `pow` per call.
/// Inlined into both callers so each gets its own straight-line copy.
#[inline(always)]
fn inverse_param(function: u16, p: &[f64; 7], y: f64, brk: Option<f64>) -> f64 {
    let [g, a, b, c, _, e, f] = *p;
    if g.abs() < 1e-9 {
        return 0.0;
    }
    let ig = 1.0 / g;
    match function {
        0 => pow_pos(y, ig),
        1 => {
            if a.abs() < 1e-9 {
                0.0
            } else {
                (pow_pos(y, ig) - b) / a
            }
        }
        2 => {
            if a.abs() < 1e-9 {
                0.0
            } else if y >= c {
                (pow_pos(y - c, ig) - b) / a
            } else {
                -b / a
            }
        }
        3 => {
            let disc = brk.unwrap_or_else(|| inverse_break(function, p));
            if y >= disc {
                if a.abs() < 1e-9 {
                    0.0
                } else {
                    (pow_pos(y, ig) - b) / a
                }
            } else if c.abs() < 1e-9 {
                0.0
            } else {
                y / c
            }
        }
        4 => {
            let disc = brk.unwrap_or_else(|| inverse_break(function, p));
            if y >= disc {
                if a.abs() < 1e-9 {
                    0.0
                } else {
                    (pow_pos(y - e, ig) - b) / a
                }
            } else if c.abs() < 1e-9 {
                0.0
            } else {
                (y - f) / c
            }
        }
        _ => y,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn srgb_inverse_round_trips() {
        let c = Curve::srgb();
        for i in 0..=1000 {
            let x = f64::from(i) / 1000.0;
            let y = c.eval(x);
            assert!((c.eval_inverse(y) - x).abs() < 1e-6, "x={x}");
        }
    }

    #[test]
    fn prepared_inverse_matches_eval_inverse_bit_for_bit() {
        let curves = [
            Curve::srgb(),
            Curve::rec709(),
            Curve::parametric(4, &[2.2, 0.9, 0.1, 0.08, 0.05, 0.02, 0.01]).unwrap(),
            Curve::parametric(3, &[1.0, 1.0, 0.0, 1.0, 0.5]).unwrap(),
            // Degenerate parameters take the early-out arms.
            Curve::parametric(3, &[2.4, 0.0, 0.1, 0.0, 0.2]).unwrap(),
            Curve::parametric(4, &[0.0, 1.0, 0.0, 0.5, 0.3, 0.0, 0.0]).unwrap(),
        ];
        for c in &curves {
            let inv = InvCurve::new(c).expect("two-segment curve");
            let mut ys: Vec<f64> = (0..=20_000).map(|i| f64::from(i) / 20_000.0).collect();
            ys.extend([-1.0, -0.0, 1.5, f64::NAN, f64::INFINITY, 1e-300, 0.04045]);
            if let Curve::Parametric { function, params } = c {
                let b = inverse_break(*function, params);
                ys.extend([b, b - f64::EPSILON, b + f64::EPSILON]);
            }
            for y in ys {
                assert_eq!(inv.eval(y).to_bits(), c.eval_inverse(y).to_bits(), "y={y}");
            }
        }
        assert!(InvCurve::new(&Curve::gamma(2.2)).is_none());
        assert!(InvCurve::new(&Curve::Table(vec![0, 65535])).is_none());
    }

    #[test]
    fn in_line_segments_match_the_curve_bit_for_bit() {
        let mut vs: Vec<f64> = (0..=40_000).map(|i| f64::from(i) / 40_000.0).collect();
        vs.extend([
            -1.0,
            -0.0,
            1.5,
            f64::NAN,
            f64::INFINITY,
            f64::NEG_INFINITY,
            1e-300,
            0.04045,
            0.081,
        ]);
        let regular = [
            Curve::srgb(),
            Curve::rec709(),
            Curve::parametric(3, &[1.8, 0.9, 0.1, 0.25, 0.3]).unwrap(),
            // A negative offset: the power segment starts below zero.
            Curve::parametric(3, &[2.2, 1.2, -0.2, 0.1, 0.1]).unwrap(),
        ];
        for c in &regular {
            let (f, inv) = (FwdSeg::new(c).unwrap(), InvCurve::new(c).unwrap());
            let i = InvSeg::new(&inv).unwrap();
            let Curve::Parametric { function, params } = c else {
                unreachable!()
            };
            let brk = inverse_break(*function, params);
            let d = params[4];
            let near = [brk, d].map(|v| {
                [
                    v,
                    f64::from_bits(v.to_bits().wrapping_sub(1)),
                    f64::from_bits(v.to_bits() + 1),
                ]
            });
            for &v in vs.iter().chain(near.iter().flatten()) {
                assert_eq!(f.eval(v).to_bits(), c.eval(v).to_bits(), "forward {v}");
                assert_eq!(i.eval(v).to_bits(), inv.eval(v).to_bits(), "inverse {v}");
                assert_eq!(
                    i.eval(v).to_bits(),
                    c.eval_inverse(v).to_bits(),
                    "inverse {v}"
                );
            }
        }
        // Curves that take other arms keep the general evaluation.
        let unit = Curve::parametric(3, &[1.0, 1.0, 0.0, 1.0, 0.5]).unwrap();
        assert!(FwdSeg::new(&unit).is_none());
        assert!(InvSeg::new(&InvCurve::new(&unit).unwrap()).is_none());
        for degenerate in [
            Curve::parametric(3, &[2.4, 0.0, 0.1, 0.5, 0.2]).unwrap(),
            Curve::parametric(3, &[2.4, 1.0, 0.1, 0.0, 0.2]).unwrap(),
            Curve::parametric(3, &[0.0, 1.0, 0.0, 0.5, 0.3]).unwrap(),
        ] {
            assert!(InvSeg::new(&InvCurve::new(&degenerate).unwrap()).is_none());
        }
        let f4 = Curve::parametric(4, &[2.2, 0.9, 0.1, 0.08, 0.05, 0.02, 0.01]).unwrap();
        assert!(FwdSeg::new(&f4).is_none());
        assert!(InvSeg::new(&InvCurve::new(&f4).unwrap()).is_none());
        assert!(FwdSeg::new(&Curve::gamma(2.2)).is_none());
    }

    #[test]
    fn sampled_matches_the_table_curve_bit_for_bit() {
        let mut xs: Vec<f64> = (0..=50_000).map(|i| f64::from(i) / 50_000.0).collect();
        xs.extend([
            -1.0,
            -0.0,
            1.5,
            f64::NAN,
            f64::INFINITY,
            f64::NEG_INFINITY,
            1e-300,
            1.0 - f64::EPSILON,
        ]);
        let mut s = 0x1234_5678u32;
        let mut noise = |n: usize| -> Vec<u16> {
            (0..n)
                .map(|_| {
                    s ^= s << 13;
                    s ^= s >> 17;
                    s ^= s << 5;
                    (s >> 16) as u16
                })
                .collect()
        };
        let tables = [
            vec![0, 65535],
            vec![65535, 0],
            vec![0, 0, 0],
            vec![65535; 5],
            vec![7, 65535, 0, 65535, 1],
            (0..256).map(|i| (i * 257) as u16).collect(),
            (0..4096u32).map(|i| (i * 65535 / 4095) as u16).collect(),
            (0..1024u32).rev().map(|i| (i * 64) as u16).collect(),
            noise(17),
            noise(4096),
        ];
        for t in tables {
            let c = Curve::Table(t.clone());
            let s = Sampled::new(&c).expect("table");
            let n = t.len();
            let grid: Vec<f64> = (0..n)
                .flat_map(|i| {
                    let x = i as f64 / (n - 1) as f64;
                    [
                        x,
                        f64::from_bits(x.to_bits().wrapping_sub(1)),
                        f64::from_bits(x.to_bits() + 1),
                    ]
                })
                .collect();
            for &x in xs.iter().chain(&grid) {
                assert_eq!(s.eval(x).to_bits(), c.eval(x).to_bits(), "n={n} x={x}");
            }
        }
        for c in [
            Curve::identity(),
            Curve::Table(vec![300]),
            Curve::srgb(),
            Curve::gamma(2.2),
        ] {
            assert!(Sampled::new(&c).is_none());
        }
    }

    #[test]
    fn table_inverse_round_trips() {
        let t: Vec<u16> = (0..256)
            .map(|i| ((f64::from(i) / 255.0).powf(2.2) * 65535.0).round() as u16)
            .collect();
        let c = Curve::Table(t);
        for i in 10..=1000 {
            let x = f64::from(i) / 1000.0;
            assert!((c.eval_inverse(c.eval(x)) - x).abs() < 2e-3, "x={x}");
        }
        let desc = Curve::Table(vec![65535, 32768, 0]);
        assert!((desc.eval_inverse(desc.eval(0.3)) - 0.3).abs() < 1e-4);
    }
}
