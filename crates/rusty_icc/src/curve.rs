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
            Self::Parametric { function, params } => inverse_param(*function, params, y),
        };
        clamp01(x)
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
            // cast truncation is the floor (no libm call).
            let i = (pos as usize).min(n - 2);
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

fn inverse_param(function: u16, p: &[f64; 7], y: f64) -> f64 {
    let [g, a, b, c, d, e, f] = *p;
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
            let disc = pow_pos(a * d + b, g);
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
            let disc = pow_pos(a * d + b, g) + e;
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
