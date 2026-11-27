//! IEEE 754 binary16 ("half") conversions.
//!
//! EXR stores half samples as raw 16-bit patterns, and this crate hands them
//! back untouched (`Samples::F16` holds the bits), so no decode path ever
//! rounds. These helpers are for callers who want `f32`.
//!
//! [`f32_to_f16`] rounds to nearest, ties to even, keeps the sign of zero,
//! overflows to infinity and keeps NaN a NaN (quiet bit set, high payload
//! bits kept) — the same results as the `half` crate and as Imath's `half`
//! for every input (the tests check all 2^32 patterns against a reference
//! formulation in release builds).

/// Half bits to `f32`. Exact for every input (NaN payloads are kept and the
/// quiet bit is set, as F16C hardware does).
#[inline]
#[must_use]
pub fn f16_to_f32(h: u16) -> f32 {
    // Rebias the exponent; subnormals renormalise through one exact float
    // subtraction, infinities and NaNs get the top exponent.
    const MAGIC: u32 = 113 << 23;
    let shifted = u32::from(h & 0x7fff) << 13;
    let exp = shifted & 0x0f80_0000;
    let mut o = shifted + ((127 - 15) << 23);
    if exp == 0x0f80_0000 {
        o += (128 - 16) << 23;
        if h & 0x3ff != 0 {
            o |= 0x0040_0000;
        }
    } else if exp == 0 {
        o += 1 << 23;
        o = (f32::from_bits(o) - f32::from_bits(MAGIC)).to_bits();
    }
    f32::from_bits(o | (u32::from(h & 0x8000) << 16))
}

/// `f32` to half bits, rounding to nearest (ties to even).
#[inline]
#[must_use]
pub fn f32_to_f16(f: f32) -> u16 {
    const INF: u32 = 255 << 23;
    const HALF_OVER: u32 = (127 + 16) << 23; // 65536.0
    const DENORM_MAGIC: u32 = ((127 - 15) + (23 - 10) + 1) << 23;
    let x = f.to_bits();
    let sign = ((x >> 16) & 0x8000) as u16;
    let a = x & 0x7fff_ffff;
    let o = if a >= HALF_OVER {
        if a > INF {
            0x7e00 | ((a & 0x007f_ffff) >> 13) as u16
        } else {
            0x7c00
        }
    } else if a < (113 << 23) {
        // Subnormal or zero half: one float addition does the rounding.
        (f32::from_bits(a) + f32::from_bits(DENORM_MAGIC))
            .to_bits()
            .wrapping_sub(DENORM_MAGIC) as u16
    } else {
        let odd = (a >> 13) & 1;
        ((a.wrapping_add(((15u32).wrapping_sub(127) << 23).wrapping_add(0xfff)) + odd) >> 13) as u16
    };
    sign | o
}

/// `f32` to half bits the way Effect-style writers prepare HDR output:
/// finite values are clamped to ±65504 (`HALF_MAX`) before rounding, so a
/// bright pixel saturates instead of becoming infinity. Infinities clamp to
/// ±65504 as well; NaN stays NaN.
#[inline]
#[must_use]
pub fn f32_to_f16_clamped(f: f32) -> u16 {
    if f.is_nan() {
        f32_to_f16(f)
    } else {
        f32_to_f16(f.clamp(-65504.0, 65504.0))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn f16_to_f32_reference(h: u16) -> f32 {
        let sign = u32::from(h & 0x8000) << 16;
        let exp = u32::from(h >> 10) & 0x1f;
        let man = u32::from(h & 0x3ff);
        let bits = if exp == 0 {
            if man == 0 {
                sign
            } else {
                let shift = man.leading_zeros() - 21;
                let man = (man << shift) & 0x3ff;
                sign | ((113 - shift) << 23) | (man << 13)
            }
        } else if exp == 0x1f {
            if man == 0 {
                sign | 0x7f80_0000
            } else {
                sign | 0x7fc0_0000 | (man << 13)
            }
        } else {
            sign | ((exp + 112) << 23) | (man << 13)
        };
        f32::from_bits(bits)
    }

    fn f32_to_f16_reference(f: f32) -> u16 {
        let x = f.to_bits();
        let sign = ((x >> 16) & 0x8000) as u16;
        let exp = x & 0x7f80_0000;
        let man = x & 0x007f_ffff;
        if exp == 0x7f80_0000 {
            let nan = if man == 0 { 0 } else { 0x0200 };
            return sign | 0x7c00 | nan | (man >> 13) as u16;
        }
        let e = (exp >> 23) as i32 - 127 + 15;
        if e >= 0x1f {
            return sign | 0x7c00;
        }
        if e <= 0 {
            if 14 - e > 24 {
                return sign;
            }
            let man = man | 0x0080_0000;
            let mut half_man = man >> (14 - e);
            let round_bit = 1u32 << (13 - e);
            if (man & round_bit) != 0 && (man & (3 * round_bit - 1)) != 0 {
                half_man += 1;
            }
            return sign | half_man as u16;
        }
        let out = u32::from(sign) | ((e as u32) << 10) | (man >> 13);
        if (man & 0x1000) != 0 && (man & (3 * 0x1000 - 1)) != 0 {
            (out + 1) as u16
        } else {
            out as u16
        }
    }

    #[test]
    fn half_to_float_matches_reference() {
        for h in 0..=u16::MAX {
            assert_eq!(
                f16_to_f32(h).to_bits(),
                f16_to_f32_reference(h).to_bits(),
                "{h:04x}"
            );
        }
    }

    #[test]
    fn float_to_half_matches_reference() {
        // Every pattern in release builds; a dense stride in debug ones.
        let step = if cfg!(debug_assertions) { 4099 } else { 1 };
        for x in (0..=u32::MAX).step_by(step) {
            let f = f32::from_bits(x);
            assert_eq!(f32_to_f16(f), f32_to_f16_reference(f), "{x:08x}");
        }
    }

    #[test]
    fn round_trip_every_half() {
        for h in 0..=u16::MAX {
            let f = f16_to_f32(h);
            let back = f32_to_f16(f);
            if f.is_nan() {
                assert!(back & 0x7c00 == 0x7c00 && back & 0x3ff != 0, "{h:04x}");
                assert_eq!(back & 0x1ff, h & 0x1ff);
            } else {
                assert_eq!(back, h, "{h:04x}");
            }
        }
    }

    #[test]
    fn rounding_ties_to_even() {
        // 1 + 2^-11 lies exactly between 1.0 and the next half; ties to even.
        assert_eq!(f32_to_f16(1.0 + 1.0 / 2048.0), 0x3c00);
        assert_eq!(f32_to_f16(1.0 + 3.0 / 2048.0), 0x3c02);
        assert_eq!(f32_to_f16(65520.0), 0x7c00);
        assert_eq!(f32_to_f16(65519.0), 0x7bff);
        assert_eq!(f32_to_f16(-0.0), 0x8000);
        assert_eq!(f32_to_f16_clamped(1.0e9), 0x7bff);
        assert_eq!(f32_to_f16_clamped(f32::NEG_INFINITY), 0xfbff);
        // Smallest subnormal and the halfway point below it.
        assert_eq!(f32_to_f16(5.960_464_5e-8), 0x0001);
        assert_eq!(f32_to_f16(2.980_232_2e-8), 0x0000);
    }
}
