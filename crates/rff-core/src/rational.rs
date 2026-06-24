//! Exact rational numbers, used for time bases and frame rates — exactly the
//! role `AVRational` plays in FFmpeg. Storing time as a rational avoids the
//! drift you get from representing e.g. 1/30000 as a float.

use std::fmt;

/// A rational number `num / den`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct Rational {
    pub num: i32,
    pub den: i32,
}

impl Rational {
    pub const ZERO: Rational = Rational { num: 0, den: 1 };

    pub const fn new(num: i32, den: i32) -> Rational {
        Rational { num, den }
    }

    /// Approximate value as `f64`. Returns `NAN` for a zero denominator rather
    /// than panicking — callers building these from untrusted headers shouldn't
    /// crash on bad input.
    pub fn as_f64(self) -> f64 {
        if self.den == 0 {
            f64::NAN
        } else {
            self.num as f64 / self.den as f64
        }
    }

    /// The reciprocal (`den / num`). Useful to flip a frame rate into a time base.
    pub fn inverse(self) -> Rational {
        Rational {
            num: self.den,
            den: self.num,
        }
    }
}

/// Defaults to `0/1` (not the derive's `0/0`, which would be a divide-by-zero
/// trap waiting to happen).
impl Default for Rational {
    fn default() -> Self {
        Rational::ZERO
    }
}

impl fmt::Display for Rational {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}/{}", self.num, self.den)
    }
}
