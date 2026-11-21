// Copyright 2020 Yevhenii Reizner
// Copyright 2026 Mata Network
//
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE-BSD-3-Clause file.

//! Scalar lane types for the raster pipeline.
//!
//! tiny-skia implements these with `core::arch` intrinsics (SSE2 on a default
//! x86-64 build, AVX/NEON/wasm when enabled) behind `unsafe`. This crate forbids
//! `unsafe`, so every type here is a plain array with lane-wise loops, which
//! LLVM auto-vectorises.
//!
//! The lane semantics deliberately reproduce tiny-skia's **SSE2** arm, which is
//! what the oracle runs on a default x86-64 build, rather than its portable
//! scalar fallback, because the two differ in edge cases that change pixels:
//!
//! - `max(a, b)` is `if a > b { a } else { b }` and `min(a, b)` is
//!   `if a < b { a } else { b }` (`maxps`/`minps`: the second operand wins on
//!   NaN or equality).
//! - `round_int` rounds half to even and `trunc_int` truncates, and both return
//!   `i32::MIN` for NaN and out-of-range input (`cvtps2dq`/`cvttps2dq`), where a
//!   Rust `as` cast would saturate.
//! - `floor` on `f32x8` is built from `trunc_int` exactly as SSE2 tiny-skia
//!   builds it (no `roundps` before SSE4.1).
//!
//! One documented divergence: `recip_fast` is an exact `1 / x` here, where SSE2
//! uses `rcpps`, a 12-bit hardware approximation that differs between CPU
//! vendors. It is only used by the `ColorDodge` and `ColorBurn` blend modes.

#![allow(non_camel_case_types)]

use core::ops::{Add, AddAssign, BitAnd, BitOr, BitXor, Div, Mul, MulAssign, Neg, Not, Shr, Sub};

#[inline(always)]
fn mask_f(b: bool) -> f32 {
    if b {
        f32::from_bits(u32::MAX)
    } else {
        0.0
    }
}

/// 2^23. From here up every `f32` is an integer.
const INTEGRAL: f32 = 8_388_608.0;

/// Rounds half to even without libm; the sign of a zero result is not kept.
///
/// For `|v| < 2^23`, adding 2^23 with `v`'s sign puts the sum in
/// `[2^23, 2^24)`, where consecutive floats are 1 apart, so the addition
/// itself rounds to an integer under the default rounding mode, and because
/// 2^23 is even a tie lands on the even integer of `v`. Subtracting 2^23
/// again is exact. Larger magnitudes are integers already, and NaN fails the
/// comparison and passes through. `f32::round_ties_even` would be a libm
/// `rintf` call per lane on a baseline x86-64 build (no SSE4.1 `roundps`).
#[inline(always)]
fn round_ties_even_magnitude(v: f32) -> f32 {
    let magic = f32::from_bits((v.to_bits() & 0x8000_0000) | INTEGRAL.to_bits());
    if v.abs() < INTEGRAL {
        (v + magic) - magic
    } else {
        v
    }
}

/// `cvtps2dq`: round half to even; NaN and out-of-range give `i32::MIN`.
///
/// As in `cvt_trunc`, the cast saturates a value below `i32::MIN` to
/// `i32::MIN` by itself, so one comparison covers NaN and `>= 2^31`.
#[inline(always)]
fn cvt_round(v: f32) -> i32 {
    let r = round_ties_even_magnitude(v);
    if r < 2_147_483_648.0 {
        r as i32
    } else {
        i32::MIN
    }
}

/// `cvttps2dq`: truncate; NaN and out-of-range give `i32::MIN`.
///
/// An `as` cast already truncates toward zero and already saturates a value
/// below `i32::MIN` to `i32::MIN`, so the only inputs it gets "wrong" are NaN
/// (0) and values at or above 2^31 (`i32::MAX`); one comparison routes both to
/// `i32::MIN`. Calling `f32::trunc` first would be a libm `truncf` call per
/// lane on a baseline x86-64 build (no SSE4.1 `roundps`), and it keeps the
/// lane loop from vectorising.
#[inline(always)]
fn cvt_trunc(v: f32) -> i32 {
    if v < 2_147_483_648.0 {
        v as i32
    } else {
        i32::MIN
    }
}

/// SSE2 tiny-skia's `floor`: truncate through `i32` (`cvttps2dq`), convert
/// back, and step down by one where the round trip went up. Computed here
/// without the conversion, which a Rust `as` cast would make lane by lane
/// (it saturates; see `round_to_u8`), in three ranges:
///
/// - `|v| < 2^23`: `(v + m) - m` with `m = copysign(2^23, v)` is the nearest
///   integer (see `round_ties_even_magnitude`), which is `floor(v)` or one
///   above it, so the same step down applies. A zero is `+0.0` in both forms.
/// - `2^23 <= |v| < 2^31`: `v` is an integer and survives the round trip.
/// - anything else (`-2^31`, out of `i32` range, NaN): the conversion gives
///   `i32::MIN`, and `-2^31 - 1` rounds back to `-2^31`.
///
/// Proven equal to the conversion form over every `f32` in the tests below.
#[inline(always)]
fn floor_through_i32(v: f32) -> f32 {
    let magnitude = f32::from_bits(v.to_bits() & 0x7fff_ffff);
    let magic = f32::from_bits((v.to_bits() & 0x8000_0000) | INTEGRAL.to_bits());
    let nearest = (v + magic) - magic;
    let small = nearest - if nearest > v { 1.0 } else { 0.0 };
    if magnitude < INTEGRAL {
        small
    } else if magnitude < 2_147_483_648.0 {
        v
    } else {
        -2_147_483_648.0
    }
}

/// `v as u16` (truncate toward zero, saturate, NaN to 0) without the
/// float-to-int conversion, which LLVM would make lane by lane (see
/// `round_to_u8`): clamp to `[0, 65535]` with NaN going to 0, then truncate as
/// `trunc_int_below_2p23` does. Proven equal to the cast over every `f32` in
/// the tests below.
#[inline(always)]
fn cast_u16(v: f32) -> u16 {
    let v = if v > 0.0 { v } else { 0.0 };
    let v = if v < 65535.0 { v } else { 65535.0 };
    let sum = v + INTEGRAL;
    let went_up = (sum - INTEGRAL > v) as u32;
    sum.to_bits()
        .wrapping_sub(INTEGRAL.to_bits())
        .wrapping_sub(went_up) as u16
}

macro_rules! lanes_bin {
    ($ty:ident, $elem:ty, $n:expr, $tr:ident, $f:ident, |$a:ident, $b:ident| $e:expr) => {
        impl $tr for $ty {
            type Output = Self;
            #[inline(always)]
            fn $f(self, rhs: Self) -> Self {
                let mut out = [<$elem>::default(); $n];
                for i in 0..$n {
                    let $a = self.0[i];
                    let $b = rhs.0[i];
                    out[i] = $e;
                }
                Self(out)
            }
        }
    };
}

// ---------------------------------------------------------------------------
// f32x8

#[derive(Clone, Copy, Debug, Default, PartialEq)]
pub struct f32x8(pub [f32; 8]);

impl f32x8 {
    #[inline(always)]
    pub fn splat(n: f32) -> Self {
        Self([n; 8])
    }

    #[inline(always)]
    fn map(self, f: impl Fn(f32) -> f32) -> Self {
        let mut o = self.0;
        for v in &mut o {
            *v = f(*v);
        }
        Self(o)
    }

    #[inline(always)]
    fn zip(self, rhs: Self, f: impl Fn(f32, f32) -> f32) -> Self {
        let mut o = self.0;
        for i in 0..8 {
            o[i] = f(self.0[i], rhs.0[i]);
        }
        Self(o)
    }

    #[inline(always)]
    pub fn floor(self) -> Self {
        self.map(floor_through_i32)
    }

    #[inline(always)]
    pub fn fract(self) -> Self {
        self - self.floor()
    }

    #[inline]
    pub fn normalize(self) -> Self {
        self.max(f32x8::default()).min(f32x8::splat(1.0))
    }

    #[inline]
    pub fn to_i32x8_bitcast(self) -> i32x8 {
        let mut o = [0i32; 8];
        for i in 0..8 {
            o[i] = self.0[i].to_bits() as i32;
        }
        i32x8(o)
    }

    #[inline]
    pub fn to_u32x8_bitcast(self) -> u32x8 {
        let mut o = [0u32; 8];
        for i in 0..8 {
            o[i] = self.0[i].to_bits();
        }
        u32x8(o)
    }

    #[inline]
    pub fn cmp_eq(self, rhs: Self) -> Self {
        self.zip(rhs, |a, b| mask_f(a == b))
    }
    #[inline]
    pub fn cmp_ne(self, rhs: Self) -> Self {
        self.zip(rhs, |a, b| mask_f(a != b))
    }
    #[inline]
    pub fn cmp_ge(self, rhs: Self) -> Self {
        self.zip(rhs, |a, b| mask_f(a >= b))
    }
    #[inline]
    pub fn cmp_gt(self, rhs: Self) -> Self {
        self.zip(rhs, |a, b| mask_f(a > b))
    }
    #[inline]
    pub fn cmp_le(self, rhs: Self) -> Self {
        self.zip(rhs, |a, b| mask_f(a <= b))
    }
    #[inline]
    pub fn cmp_lt(self, rhs: Self) -> Self {
        self.zip(rhs, |a, b| mask_f(a < b))
    }

    /// Bitwise select: `self` is a lane mask; picks `t` where set, `f` where clear.
    #[inline(always)]
    pub fn blend(self, t: Self, f: Self) -> Self {
        let mut o = [0.0f32; 8];
        for i in 0..8 {
            let m = self.0[i].to_bits();
            let n = f.0[i].to_bits();
            o[i] = f32::from_bits(n ^ ((n ^ t.0[i].to_bits()) & m));
        }
        Self(o)
    }

    #[inline]
    pub fn abs(self) -> Self {
        self.map(|v| f32::from_bits(v.to_bits() & 0x7fff_ffff))
    }

    /// `maxps` semantics.
    #[inline]
    pub fn max(self, rhs: Self) -> Self {
        self.zip(rhs, |a, b| if a > b { a } else { b })
    }

    /// `minps` semantics.
    #[inline]
    pub fn min(self, rhs: Self) -> Self {
        self.zip(rhs, |a, b| if a < b { a } else { b })
    }

    #[inline]
    pub fn is_finite(self) -> Self {
        self.map(|v| mask_f((v.to_bits() << 1) & 0xFF00_0000 != 0xFF00_0000))
    }

    #[inline]
    pub fn round(self) -> Self {
        // tiny-skia's portable round: values with |v| >= 2^23 (and NaN) pass
        // through; everything else rounds half to even and keeps the sign of
        // `v`, including on a zero result (so |v| < 0.5 is a signed zero).
        // Written without a branch and without `f32::round_ties_even` (a libm
        // `rintf` call per lane on baseline x86-64), so the lanes vectorise.
        self.map(|v| {
            let sign = v.to_bits() & 0x8000_0000;
            let r = round_ties_even_magnitude(v);
            f32::from_bits(r.to_bits() | sign)
        })
    }

    #[inline]
    pub fn round_int(self) -> i32x8 {
        let mut o = [0i32; 8];
        for i in 0..8 {
            o[i] = cvt_round(self.0[i]);
        }
        i32x8(o)
    }

    /// `round_int` for lanes already in `[0, 255]`, without a float-to-int
    /// conversion.
    ///
    /// Adding 2^23 puts the sum in `[2^23, 2^24)`, where consecutive floats
    /// are 1 apart, so the addition itself rounds to an integer (half to
    /// even, 2^23 being even), and that integer, at most 255, is the low byte
    /// of the sum's mantissa. A Rust `as` cast saturates, which LLVM lowers
    /// lane by lane on baseline x86-64 (`cvttss2si` plus two compare-and-
    /// select fix-ups per lane); this is one `addps` and one `pand`.
    ///
    /// A lane outside `[0, 255]` (or NaN) gives an unspecified byte.
    #[inline(always)]
    pub fn round_to_u8(self) -> u32x8 {
        let mut o = [0u32; 8];
        for i in 0..8 {
            o[i] = (self.0[i] + INTEGRAL).to_bits() & 0xff;
        }
        u32x8(o)
    }

    /// `trunc_int` for lanes in `[0, 2^23)`, without a float-to-int
    /// conversion (see `round_to_u8` for why that matters).
    ///
    /// `v + 2^23` is the nearest integer to `v`, plus 2^23, with that integer
    /// in the low bits of the sum's bit pattern (the sum stays in
    /// `[2^23, 2^24]`, and at 2^24 the pattern carries into the exponent as
    /// plain binary addition would). Where the sum went up, step back by one.
    ///
    /// A lane outside `[0, 2^23)` (or NaN) gives an unspecified value.
    #[inline(always)]
    pub fn trunc_int_below_2p23(self) -> i32x8 {
        let mut o = [0i32; 8];
        for i in 0..8 {
            let v = self.0[i];
            let sum = v + INTEGRAL;
            let went_up = (sum - INTEGRAL > v) as u32;
            o[i] = sum
                .to_bits()
                .wrapping_sub(INTEGRAL.to_bits())
                .wrapping_sub(went_up) as i32;
        }
        i32x8(o)
    }

    #[inline]
    pub fn trunc_int(self) -> i32x8 {
        let mut o = [0i32; 8];
        for i in 0..8 {
            o[i] = cvt_trunc(self.0[i]);
        }
        i32x8(o)
    }

    /// Exact reciprocal. SSE2 tiny-skia uses the `rcpps` approximation here.
    #[inline]
    pub fn recip_fast(self) -> Self {
        self.map(|v| 1.0 / v)
    }

    #[inline]
    pub fn recip_sqrt(self) -> Self {
        self.map(|v| 1.0 / v.sqrt())
    }

    #[inline]
    pub fn sqrt(self) -> Self {
        self.map(f32::sqrt)
    }

    #[allow(clippy::excessive_precision)]
    #[inline]
    pub fn powf(self, exp: f32) -> Self {
        let x = self;
        // We assume sign(x) is positive so we can use vectorized i32->f32 conversions
        let e = x.to_i32x8_bitcast().to_f32x8() * f32x8::splat(1.0f32 / ((1 << 23) as f32));
        let m = (x.to_u32x8_bitcast() & u32x8::splat(0x007fffff) | u32x8::splat(0x3f000000))
            .to_f32x8_bitcast();

        let log2_x = e
            - f32x8::splat(124.225514990)
            - f32x8::splat(1.498030302) * m
            - f32x8::splat(1.725879990) / (f32x8::splat(0.3520887068) + m);

        let x = log2_x * f32x8::splat(exp);

        let f = x - x.floor();

        let mut a = x + f32x8::splat(121.274057500);
        a = a - f * f32x8::splat(1.490129070);
        a += f32x8::splat(27.728023300) / (f32x8::splat(4.84252568) - f);
        a *= f32x8::splat((1 << 23) as f32);

        let inf_bits = f32x8::splat(f32::INFINITY.to_bits() as f32);

        let x = a
            .max(f32x8::splat(0.0))
            .min(inf_bits)
            .round_int()
            .to_f32x8_bitcast();

        let skip = self.cmp_eq(f32x8::splat(0.0)) | self.cmp_eq(f32x8::splat(1.0));
        skip.blend(self, x)
    }
}

impl From<[f32; 8]> for f32x8 {
    #[inline]
    fn from(v: [f32; 8]) -> Self {
        Self(v)
    }
}

impl From<f32x8> for [f32; 8] {
    #[inline]
    fn from(v: f32x8) -> Self {
        v.0
    }
}

lanes_bin!(f32x8, f32, 8, Add, add, |a, b| a + b);
lanes_bin!(f32x8, f32, 8, Sub, sub, |a, b| a - b);
lanes_bin!(f32x8, f32, 8, Mul, mul, |a, b| a * b);
lanes_bin!(f32x8, f32, 8, Div, div, |a, b| a / b);
lanes_bin!(f32x8, f32, 8, BitAnd, bitand, |a, b| f32::from_bits(
    a.to_bits() & b.to_bits()
));
lanes_bin!(f32x8, f32, 8, BitOr, bitor, |a, b| f32::from_bits(
    a.to_bits() | b.to_bits()
));
lanes_bin!(f32x8, f32, 8, BitXor, bitxor, |a, b| f32::from_bits(
    a.to_bits() ^ b.to_bits()
));

impl AddAssign for f32x8 {
    #[inline]
    fn add_assign(&mut self, rhs: f32x8) {
        *self = *self + rhs;
    }
}

impl MulAssign for f32x8 {
    #[inline]
    fn mul_assign(&mut self, rhs: f32x8) {
        *self = *self * rhs;
    }
}

impl Neg for f32x8 {
    type Output = Self;
    #[inline]
    fn neg(self) -> Self {
        Self::default() - self
    }
}

impl Not for f32x8 {
    type Output = Self;
    #[inline]
    fn not(self) -> Self {
        self.map(|v| f32::from_bits(!v.to_bits()))
    }
}

// ---------------------------------------------------------------------------
// i32x8

#[derive(Clone, Copy, Debug, Default, PartialEq)]
pub struct i32x8(pub [i32; 8]);

impl i32x8 {
    #[inline]
    pub fn splat(n: i32) -> Self {
        Self([n; 8])
    }

    #[inline]
    pub fn blend(self, t: Self, f: Self) -> Self {
        let mut o = [0i32; 8];
        for i in 0..8 {
            o[i] = f.0[i] ^ ((f.0[i] ^ t.0[i]) & self.0[i]);
        }
        Self(o)
    }

    #[inline]
    fn cmp(self, rhs: Self, f: impl Fn(i32, i32) -> bool) -> Self {
        let mut o = [0i32; 8];
        for i in 0..8 {
            o[i] = if f(self.0[i], rhs.0[i]) { -1 } else { 0 };
        }
        Self(o)
    }

    #[inline]
    pub fn cmp_eq(self, rhs: Self) -> Self {
        self.cmp(rhs, |a, b| a == b)
    }
    #[inline]
    pub fn cmp_gt(self, rhs: Self) -> Self {
        self.cmp(rhs, |a, b| a > b)
    }
    #[inline]
    pub fn cmp_lt(self, rhs: Self) -> Self {
        self.cmp(rhs, |a, b| a < b)
    }

    #[inline]
    pub fn to_f32x8(self) -> f32x8 {
        let mut o = [0.0f32; 8];
        for i in 0..8 {
            o[i] = self.0[i] as f32;
        }
        f32x8(o)
    }

    #[inline]
    pub fn to_u32x8_bitcast(self) -> u32x8 {
        let mut o = [0u32; 8];
        for i in 0..8 {
            o[i] = self.0[i] as u32;
        }
        u32x8(o)
    }

    #[inline]
    pub fn to_f32x8_bitcast(self) -> f32x8 {
        let mut o = [0.0f32; 8];
        for i in 0..8 {
            o[i] = f32::from_bits(self.0[i] as u32);
        }
        f32x8(o)
    }
}

impl From<[i32; 8]> for i32x8 {
    #[inline]
    fn from(v: [i32; 8]) -> Self {
        Self(v)
    }
}

impl From<i32x8> for [i32; 8] {
    #[inline]
    fn from(v: i32x8) -> Self {
        v.0
    }
}

lanes_bin!(i32x8, i32, 8, Add, add, |a, b| a.wrapping_add(b));
lanes_bin!(i32x8, i32, 8, Mul, mul, |a, b| a.wrapping_mul(b));
lanes_bin!(i32x8, i32, 8, BitAnd, bitand, |a, b| a & b);
lanes_bin!(i32x8, i32, 8, BitOr, bitor, |a, b| a | b);
lanes_bin!(i32x8, i32, 8, BitXor, bitxor, |a, b| a ^ b);

// ---------------------------------------------------------------------------
// u32x8

#[derive(Clone, Copy, Debug, Default, PartialEq)]
pub struct u32x8(pub [u32; 8]);

impl u32x8 {
    #[inline]
    pub fn splat(n: u32) -> Self {
        Self([n; 8])
    }

    #[inline]
    pub fn to_i32x8_bitcast(self) -> i32x8 {
        let mut o = [0i32; 8];
        for i in 0..8 {
            o[i] = self.0[i] as i32;
        }
        i32x8(o)
    }

    #[inline]
    pub fn to_f32x8_bitcast(self) -> f32x8 {
        let mut o = [0.0f32; 8];
        for i in 0..8 {
            o[i] = f32::from_bits(self.0[i]);
        }
        f32x8(o)
    }

    #[inline]
    pub fn cmp_eq(self, rhs: Self) -> Self {
        let mut o = [0u32; 8];
        for i in 0..8 {
            o[i] = if self.0[i] == rhs.0[i] { u32::MAX } else { 0 };
        }
        Self(o)
    }

    #[inline]
    pub fn shl<const RHS: i32>(self) -> Self {
        let mut o = self.0;
        for v in &mut o {
            *v = if (0..32).contains(&RHS) { *v << RHS } else { 0 };
        }
        Self(o)
    }

    #[inline]
    pub fn shr<const RHS: i32>(self) -> Self {
        let mut o = self.0;
        for v in &mut o {
            *v = if (0..32).contains(&RHS) { *v >> RHS } else { 0 };
        }
        Self(o)
    }
}

impl From<[u32; 8]> for u32x8 {
    #[inline]
    fn from(v: [u32; 8]) -> Self {
        Self(v)
    }
}

impl From<u32x8> for [u32; 8] {
    #[inline]
    fn from(v: u32x8) -> Self {
        v.0
    }
}

lanes_bin!(u32x8, u32, 8, Add, add, |a, b| a.wrapping_add(b));
lanes_bin!(u32x8, u32, 8, BitAnd, bitand, |a, b| a & b);
lanes_bin!(u32x8, u32, 8, BitOr, bitor, |a, b| a | b);

impl Not for u32x8 {
    type Output = Self;
    #[inline]
    fn not(self) -> Self {
        let mut o = self.0;
        for v in &mut o {
            *v = !*v;
        }
        Self(o)
    }
}

// ---------------------------------------------------------------------------
// f32x16 (two f32x8 halves, as in tiny-skia)

#[derive(Clone, Copy, Debug, Default, PartialEq)]
pub struct f32x16(pub f32x8, pub f32x8);

impl f32x16 {
    pub fn splat(n: f32) -> Self {
        Self(f32x8::splat(n), f32x8::splat(n))
    }

    pub fn abs(&self) -> Self {
        Self(self.0.abs(), self.1.abs())
    }

    pub fn cmp_gt(&self, rhs: &Self) -> Self {
        Self(self.0.cmp_gt(rhs.0), self.1.cmp_gt(rhs.1))
    }

    pub fn blend(self, t: Self, f: Self) -> Self {
        Self(self.0.blend(t.0, f.0), self.1.blend(t.1, f.1))
    }

    pub fn normalize(&self) -> Self {
        Self(self.0.normalize(), self.1.normalize())
    }

    pub fn floor(&self) -> Self {
        // Yes, Skia does it in the same way.
        let roundtrip = self.round();
        roundtrip
            - roundtrip
                .cmp_gt(self)
                .blend(f32x16::splat(1.0), f32x16::splat(0.0))
    }

    pub fn sqrt(&self) -> Self {
        Self(self.0.sqrt(), self.1.sqrt())
    }

    pub fn round(&self) -> Self {
        Self(self.0.round(), self.1.round())
    }

    /// The body of `save_to_u16x16`, for a caller that wants it in line.
    ///
    /// One body cannot serve both lowp gradient stages: in line, the 2-stop
    /// stage gains (grad_linear2 -32.7% against -30.8% with a call) and the
    /// multi-stop stage, already short of registers, loses (grad_linear5
    /// -14.3% against -15.4%). Hence two names.
    #[inline(always)]
    pub fn save_to_u16x16_inline(&self, dst: &mut u16x16) {
        // Do not use to_i32x8, because it involves rounding,
        // and Skia cast's without it.
        for i in 0..8 {
            dst.0[i] = cast_u16(self.0 .0[i]);
            dst.0[i + 8] = cast_u16(self.1 .0[i]);
        }
    }

    /// The out-of-line handle on the same body.
    #[inline(never)]
    pub fn save_to_u16x16(&self, dst: &mut u16x16) {
        self.save_to_u16x16_inline(dst);
    }
}

impl Add for f32x16 {
    type Output = Self;
    fn add(self, rhs: Self) -> Self {
        Self(self.0 + rhs.0, self.1 + rhs.1)
    }
}

impl Sub for f32x16 {
    type Output = Self;
    fn sub(self, rhs: Self) -> Self {
        Self(self.0 - rhs.0, self.1 - rhs.1)
    }
}

impl Mul for f32x16 {
    type Output = Self;
    fn mul(self, rhs: Self) -> Self {
        Self(self.0 * rhs.0, self.1 * rhs.1)
    }
}

// ---------------------------------------------------------------------------
// u16x16

#[derive(Copy, Clone, PartialEq, Default, Debug)]
pub struct u16x16(pub [u16; 16]);

impl u16x16 {
    #[inline(always)]
    pub fn splat(n: u16) -> Self {
        Self([n; 16])
    }

    #[inline(always)]
    pub fn as_slice(&self) -> &[u16; 16] {
        &self.0
    }

    #[inline(always)]
    pub fn min(&self, rhs: &Self) -> Self {
        let mut o = self.0;
        for i in 0..16 {
            o[i] = o[i].min(rhs.0[i]);
        }
        Self(o)
    }

    #[inline(always)]
    pub fn max(&self, rhs: &Self) -> Self {
        let mut o = self.0;
        for i in 0..16 {
            o[i] = o[i].max(rhs.0[i]);
        }
        Self(o)
    }

    #[inline(always)]
    pub fn cmp_le(&self, rhs: &Self) -> Self {
        let mut o = [0u16; 16];
        for i in 0..16 {
            o[i] = if self.0[i] <= rhs.0[i] { !0 } else { 0 };
        }
        Self(o)
    }

    #[inline(always)]
    pub fn blend(self, t: Self, e: Self) -> Self {
        (t & self) | (e & !self)
    }
}

// tiny-skia uses plain `+`/`-`/`*` here, which wrap in a release build; the
// explicit wrapping ops keep debug builds (and the oracle tests) identical.
lanes_bin!(u16x16, u16, 16, Add, add, |a, b| a.wrapping_add(b));
lanes_bin!(u16x16, u16, 16, Sub, sub, |a, b| a.wrapping_sub(b));
lanes_bin!(u16x16, u16, 16, Mul, mul, |a, b| a.wrapping_mul(b));
lanes_bin!(u16x16, u16, 16, Div, div, |a, b| a
    .checked_div(b)
    .unwrap_or(0));
lanes_bin!(u16x16, u16, 16, BitAnd, bitand, |a, b| a & b);
lanes_bin!(u16x16, u16, 16, BitOr, bitor, |a, b| a | b);
lanes_bin!(u16x16, u16, 16, Shr, shr, |a, b| a
    .checked_shr(u32::from(b))
    .unwrap_or(0));

impl Not for u16x16 {
    type Output = Self;
    #[inline(always)]
    fn not(self) -> Self {
        let mut o = self.0;
        for v in &mut o {
            *v = !*v;
        }
        Self(o)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The lane conversions as they were first written, on top of libm
    /// `trunc` and `rint`. They stay here as the oracle for the libm-free
    /// forms above.
    fn cvt_trunc_ref(v: f32) -> i32 {
        let r = v.trunc();
        if r >= -2_147_483_648.0 && r < 2_147_483_648.0 {
            r as i32
        } else {
            i32::MIN
        }
    }

    fn cvt_round_ref(v: f32) -> i32 {
        let r = v.round_ties_even();
        if r >= -2_147_483_648.0 && r < 2_147_483_648.0 {
            r as i32
        } else {
            i32::MIN
        }
    }

    /// `f32x8::round` as first written (tiny-skia's portable `round`).
    fn round_ref(v: f32) -> f32 {
        let e = (v.to_bits() >> 23) & 0xff;
        if e >= 0x7f + 23 {
            v
        } else if e < 0x7f - 1 {
            v * 0.0
        } else {
            v.round_ties_even()
        }
    }

    /// `f32x8::floor` as first written (SSE2 tiny-skia's `floor`).
    fn floor_ref(v: f32) -> f32 {
        let roundtrip = cvt_trunc_ref(v) as f32;
        roundtrip - if roundtrip > v { 1.0 } else { 0.0 }
    }

    fn check(v: f32) {
        assert_eq!(
            f32x8::splat(v).floor().0[6].to_bits(),
            floor_ref(v).to_bits(),
            "floor({v:e})"
        );
        assert_eq!(cvt_trunc(v), cvt_trunc_ref(v), "cvt_trunc({v:e})");
        assert_eq!(cast_u16(v), v as u16, "cast_u16({v:e})");
        assert_eq!(cvt_round(v), cvt_round_ref(v), "cvt_round({v:e})");
        assert_eq!(
            f32x8::splat(v).round().0[3].to_bits(),
            round_ref(v).to_bits(),
            "round({v:e})"
        );
        // The highp store: clamp to [0, 1], scale to [0, 255], round to a
        // byte. `round_to_u8` must give the byte `round_int` gave.
        let x = f32x8::splat(v).max(f32x8::default()).min(f32x8::splat(1.0)) * f32x8::splat(255.0);
        assert_eq!(
            x.round_to_u8().0[5],
            cvt_round_ref(x.0[5]) as u8 as u32,
            "unnorm({v:e})"
        );
    }

    /// `trunc_int_below_2p23` against `trunc_int` on every `step`-th float in
    /// `[0, 2^23)`, and around every power of two and the top of the range.
    fn sweep_below_2p23(step: usize) {
        let check = |bits: u32| {
            let v = f32::from_bits(bits);
            assert_eq!(
                f32x8::splat(v).trunc_int_below_2p23().0[1],
                cvt_trunc_ref(v),
                "trunc_int_below_2p23({v:e})"
            );
        };
        let top = INTEGRAL.to_bits();
        for bits in (0..top).step_by(step) {
            check(bits);
        }
        for e in 0..=23 {
            let b = ((1u32 << e) as f32).to_bits();
            for d in -3i32..=3 {
                let bits = b.wrapping_add(d as u32);
                if bits < top {
                    check(bits);
                }
            }
        }
        check(0x8000_0000); // -0.0, what `max(0)` could let through
    }

    /// `round_to_u8` against `round_int` on every `step`-th float in `[0, 255]`.
    fn sweep_bytes(step: usize) {
        for bits in (0..=255.0f32.to_bits()).step_by(step) {
            let x = f32::from_bits(bits);
            assert_eq!(
                f32x8::splat(x).round_to_u8().0[2],
                cvt_round_ref(x) as u32,
                "round_to_u8({x:e})"
            );
        }
        for k in 0..=255u32 {
            for d in -2i32..=2 {
                let x = f32::from_bits((k as f32 + 0.5).to_bits().wrapping_add(d as u32));
                if (0.0..=255.0).contains(&x) {
                    assert_eq!(f32x8::splat(x).round_to_u8().0[0], cvt_round_ref(x) as u32);
                }
            }
        }
    }

    /// Every `step`-th bit pattern, plus the neighbourhoods where rounding,
    /// range and NaN handling change.
    fn sweep(step: usize) {
        for bits in (0..=u32::MAX).step_by(step) {
            check(f32::from_bits(bits));
        }
        let edges = [
            0.0f32,
            0.5,
            1.0,
            1.5,
            2.5,
            255.0,
            254.5,
            4_194_304.0,
            8_388_607.5,
            8_388_608.0,
            65_534.5,
            65_535.0,
            65_535.5,
            65_536.0,
            16_777_216.0,
            2_147_483_648.0,
            4_294_967_296.0,
            f32::MIN_POSITIVE,
            f32::MAX,
            f32::INFINITY,
            f32::NAN,
        ];
        for e in edges {
            for sign in [0u32, 0x8000_0000] {
                for d in -3i32..=3 {
                    check(f32::from_bits((e.to_bits() | sign).wrapping_add(d as u32)));
                }
            }
        }
    }

    #[test]
    fn lane_conversions_match_libm_sampled() {
        sweep(4099);
        sweep_bytes(1021);
        sweep_below_2p23(1021);
    }

    /// All 2^32 bit patterns. `cargo test --release -- --ignored lane_conversions`.
    #[test]
    #[ignore = "exhaustive over every f32 bit pattern; run in release"]
    fn lane_conversions_match_libm_exhaustive() {
        sweep(1);
        sweep_bytes(1);
        sweep_below_2p23(1);
    }
}
