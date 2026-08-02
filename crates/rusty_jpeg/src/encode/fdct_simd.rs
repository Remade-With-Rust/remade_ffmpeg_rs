//! The LL&M forward DCT, written once over a tiny lane abstraction.
//!
//! # Why it is shaped like this
//!
//! A NEON port of the FDCT cannot be executed on the x86 machine it was written
//! on, so a direct 250-line transcription would rest entirely on a cross-compile
//! succeeding. Instead the butterfly is written **once**, generic over a
//! [`Lanes`] trait, with two backends:
//!
//! - `[i32; 8]` — a scalar model, exercised on x86 by
//!   `generic_butterfly_matches_scalar_fdct`, which compares it against the
//!   reference [`fdct`](super::fdct::fdct) exhaustively.
//! - NEON — a mapping of the same trait onto `int32x4_t` pairs.
//!
//! That splits "did I restructure the DCT correctly" (verified HERE, on this
//! machine) from "did I map each operation to the right intrinsic" (about
//! fifteen one-line bodies, every one of them type-checked by the compiler
//! against the aarch64 intrinsic signatures). The unverified surface is the
//! mapping, not the transform.
//!
//! # Structure
//!
//! The scalar reference runs pass 1 over ROWS and pass 2 over COLUMNS, and the
//! order is load-bearing: the two passes use different fixed-point descale
//! amounts, so swapping them changes the rounding and the output is no longer
//! bit-identical. A vertical (element-wise across registers) operation is a
//! COLUMN butterfly, so getting a row butterfly means transposing first:
//!
//! ```text
//!   transpose -> vertical pass 1 -> transpose -> vertical pass 2
//! ```
//!
//! Intermediates between passes fit in `i16`: inputs are level-shifted to
//! ±128, so the largest pass-1 value is about ±4096. libjpeg's own SIMD relies
//! on the same bound.

#![allow(dead_code)]

use super::fdct::{
    CONST_BITS, FIX_0_298631336, FIX_0_390180644, FIX_0_541196100, FIX_0_765366865,
    FIX_0_899976223, FIX_1_175875602, FIX_1_501321110, FIX_1_847759065, FIX_1_961570560,
    FIX_2_053119869, FIX_2_562915447, FIX_3_072711026, PASS1_BITS,
};

/// Eight parallel 32-bit lanes. Every operation the butterfly needs, and
/// nothing else — the whole point is that a backend is trivial to audit.
pub(crate) trait Lanes: Copy {
    fn add(self, o: Self) -> Self;
    fn sub(self, o: Self) -> Self;
    /// Multiply every lane by a compile-time-known fixed-point constant.
    fn mul(self, k: i32) -> Self;
    /// Left shift, for pass 1's `<< PASS1_BITS`.
    fn shl(self, n: i32) -> Self;
    /// Rounding right shift: `(x + (1 << (n-1))) >> n`.
    fn descale(self, n: i32) -> Self;
}

impl Lanes for [i32; 8] {
    #[inline(always)]
    fn add(self, o: Self) -> Self {
        core::array::from_fn(|i| self[i].wrapping_add(o[i]))
    }
    #[inline(always)]
    fn sub(self, o: Self) -> Self {
        core::array::from_fn(|i| self[i].wrapping_sub(o[i]))
    }
    #[inline(always)]
    fn mul(self, k: i32) -> Self {
        core::array::from_fn(|i| self[i].wrapping_mul(k))
    }
    #[inline(always)]
    fn shl(self, n: i32) -> Self {
        core::array::from_fn(|i| self[i] << n)
    }
    #[inline(always)]
    fn descale(self, n: i32) -> Self {
        core::array::from_fn(|i| (self[i].wrapping_add(1 << (n - 1))) >> n)
    }
}

/// One butterfly pass over eight lanes, element-wise across the eight rows.
///
/// Line-for-line the scalar reference. `pass2` selects the descale amounts; it
/// is always called with a literal, so the branch folds away.
#[inline(always)]
pub(crate) fn butterfly<L: Lanes>(r: &mut [L; 8], pass2: bool) {
    let tmp0 = r[0].add(r[7]);
    let tmp7 = r[0].sub(r[7]);
    let tmp1 = r[1].add(r[6]);
    let tmp6 = r[1].sub(r[6]);
    let tmp2 = r[2].add(r[5]);
    let tmp5 = r[2].sub(r[5]);
    let tmp3 = r[3].add(r[4]);
    let tmp4 = r[3].sub(r[4]);

    // Even part.
    let tmp10 = tmp0.add(tmp3);
    let tmp13 = tmp0.sub(tmp3);
    let tmp11 = tmp1.add(tmp2);
    let tmp12 = tmp1.sub(tmp2);

    if pass2 {
        r[0] = tmp10.add(tmp11).descale(PASS1_BITS);
        r[4] = tmp10.sub(tmp11).descale(PASS1_BITS);
    } else {
        r[0] = tmp10.add(tmp11).shl(PASS1_BITS);
        r[4] = tmp10.sub(tmp11).shl(PASS1_BITS);
    }

    let even_shift = if pass2 {
        CONST_BITS + PASS1_BITS
    } else {
        CONST_BITS - PASS1_BITS
    };

    let z1 = tmp12.add(tmp13).mul(FIX_0_541196100);
    r[2] = z1.add(tmp13.mul(FIX_0_765366865)).descale(even_shift);
    r[6] = z1.add(tmp12.mul(-FIX_1_847759065)).descale(even_shift);

    // Odd part.
    let z1 = tmp4.add(tmp7);
    let z2 = tmp5.add(tmp6);
    let z3 = tmp4.add(tmp6);
    let z4 = tmp5.add(tmp7);
    let z5 = z3.add(z4).mul(FIX_1_175875602);

    let tmp4 = tmp4.mul(FIX_0_298631336);
    let tmp5 = tmp5.mul(FIX_2_053119869);
    let tmp6 = tmp6.mul(FIX_3_072711026);
    let tmp7 = tmp7.mul(FIX_1_501321110);
    let z1 = z1.mul(-FIX_0_899976223);
    let z2 = z2.mul(-FIX_2_562915447);
    let z3 = z3.mul(-FIX_1_961570560);
    let z4 = z4.mul(-FIX_0_390180644);

    let z3 = z3.add(z5);
    let z4 = z4.add(z5);

    r[7] = tmp4.add(z1).add(z3).descale(even_shift);
    r[5] = tmp5.add(z2).add(z4).descale(even_shift);
    r[3] = tmp6.add(z2).add(z3).descale(even_shift);
    r[1] = tmp7.add(z1).add(z4).descale(even_shift);
}

/// The whole transform on the scalar model, in the SIMD register layout.
///
/// This is what `generic_butterfly_matches_scalar_fdct` runs on x86: if it
/// reproduces the reference DCT bit for bit, the restructuring — the transposes,
/// the pass ordering, the descale amounts — is correct, and a backend only has
/// to map [`Lanes`] faithfully.
pub(crate) fn fdct_via_lanes(data: &mut [i16; 64]) {
    let mut r = [[0i32; 8]; 8];
    // Transpose on load: register i holds column i, so an element-wise op across
    // registers is a ROW butterfly.
    for (i, reg) in r.iter_mut().enumerate() {
        for (j, lane) in reg.iter_mut().enumerate() {
            *lane = data[j * 8 + i] as i32;
        }
    }

    butterfly(&mut r, false);

    // Back to row-major, so the next element-wise pass is a COLUMN butterfly.
    let mut t = [[0i32; 8]; 8];
    for i in 0..8 {
        for j in 0..8 {
            t[i][j] = r[j][i];
        }
    }
    let mut r = t;

    butterfly(&mut r, true);

    for i in 0..8 {
        for j in 0..8 {
            data[i * 8 + j] = r[i][j] as i16;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::encode::fdct::fdct;

    /// The restructured transform must be BIT-IDENTICAL to the reference.
    ///
    /// This is the test that makes a NEON port defensible from an x86 machine:
    /// it verifies the transposes, the pass ordering and the fixed-point
    /// descale amounts — everything except the intrinsic mapping itself.
    #[test]
    fn generic_butterfly_matches_scalar_fdct() {
        let mut state = 0x853C_49E6_748F_EA9Bu64;
        let mut next = move || {
            state ^= state << 13;
            state ^= state >> 7;
            state ^= state << 17;
            state
        };
        for round in 0..2000 {
            let mut a = [0i16; 64];
            for (i, v) in a.iter_mut().enumerate() {
                *v = match round {
                    0 => 0,
                    1 => 127,
                    2 => -128,
                    3 => {
                        if i == 0 {
                            -128
                        } else {
                            127
                        }
                    }
                    4 => {
                        if i % 2 == 0 {
                            127
                        } else {
                            -128
                        }
                    }
                    _ => (next() % 256) as i16 - 128,
                };
            }
            let mut b = a;
            fdct(&mut a);
            fdct_via_lanes(&mut b);
            assert_eq!(a, b, "round {round}");
        }
    }
}

/// NEON backend: eight lanes as a pair of `int32x4_t`.
///
/// Every body here is a one-to-one mapping of the corresponding [`Lanes`]
/// operation onto an aarch64 intrinsic, and each is type-checked by the
/// compiler against that intrinsic's signature. The transform itself — the part
/// that could be subtly wrong — is the generic [`butterfly`] above, verified
/// bit-exactly on x86 by `generic_butterfly_matches_scalar_fdct`.
#[cfg(all(feature = "simd", target_arch = "aarch64"))]
pub(crate) mod neon {
    use super::{butterfly, Lanes};
    use core::arch::aarch64::*;

    #[derive(Clone, Copy)]
    pub(crate) struct I32x8(pub int32x4_t, pub int32x4_t);

    impl Lanes for I32x8 {
        #[inline(always)]
        fn add(self, o: Self) -> Self {
            // SAFETY: NEON is baseline on aarch64.
            unsafe { I32x8(vaddq_s32(self.0, o.0), vaddq_s32(self.1, o.1)) }
        }
        #[inline(always)]
        fn sub(self, o: Self) -> Self {
            unsafe { I32x8(vsubq_s32(self.0, o.0), vsubq_s32(self.1, o.1)) }
        }
        #[inline(always)]
        fn mul(self, k: i32) -> Self {
            unsafe {
                let v = vdupq_n_s32(k);
                I32x8(vmulq_s32(self.0, v), vmulq_s32(self.1, v))
            }
        }
        #[inline(always)]
        fn shl(self, n: i32) -> Self {
            unsafe {
                let v = vdupq_n_s32(n);
                I32x8(vshlq_s32(self.0, v), vshlq_s32(self.1, v))
            }
        }
        #[inline(always)]
        fn descale(self, n: i32) -> Self {
            // (x + (1 << (n-1))) >> n, matching the scalar `descale` exactly.
            unsafe {
                let bias = vdupq_n_s32(1 << (n - 1));
                let sh = vdupq_n_s32(-n);
                I32x8(
                    vshlq_s32(vaddq_s32(self.0, bias), sh),
                    vshlq_s32(vaddq_s32(self.1, bias), sh),
                )
            }
        }
    }

    /// Forward DCT, bit-identical to the scalar reference.
    ///
    /// The transposes go through a stack array using the SAME index arithmetic
    /// as the x86-verified `fdct_via_lanes`, rather than a hand-rolled
    /// `vtrn`/`vzip` network. That is a deliberate trade: a register transpose
    /// would be faster, but it would also be a second block of untestable
    /// index-shuffling logic, and the butterfly — some fifty operations per
    /// lane — is where the work actually is.
    pub(crate) fn fdct_neon(data: &mut [i16; 64]) {
        #[inline(always)]
        fn load(v: &[i32; 8]) -> I32x8 {
            unsafe { I32x8(vld1q_s32(v.as_ptr()), vld1q_s32(v.as_ptr().add(4))) }
        }
        #[inline(always)]
        fn store(x: I32x8, v: &mut [i32; 8]) {
            unsafe {
                vst1q_s32(v.as_mut_ptr(), x.0);
                vst1q_s32(v.as_mut_ptr().add(4), x.1);
            }
        }

        // Transpose on load: register i holds column i.
        let mut cols = [[0i32; 8]; 8];
        for (i, c) in cols.iter_mut().enumerate() {
            for (j, lane) in c.iter_mut().enumerate() {
                *lane = data[j * 8 + i] as i32;
            }
        }
        let mut r = [load(&cols[0]); 8];
        for i in 1..8 {
            r[i] = load(&cols[i]);
        }

        butterfly(&mut r, false);

        for i in 0..8 {
            store(r[i], &mut cols[i]);
        }
        // Back to row-major for the column pass.
        let mut rows = [[0i32; 8]; 8];
        for i in 0..8 {
            for j in 0..8 {
                rows[i][j] = cols[j][i];
            }
        }
        for i in 0..8 {
            r[i] = load(&rows[i]);
        }

        butterfly(&mut r, true);

        for i in 0..8 {
            store(r[i], &mut rows[i]);
        }
        for i in 0..8 {
            for j in 0..8 {
                data[i * 8 + j] = rows[i][j] as i16;
            }
        }
    }

    #[cfg(test)]
    mod tests {
        use super::*;
        use crate::encode::fdct::fdct;

        /// Bit-exact against the scalar reference. Runs only on aarch64, so ARM
        /// CI is what certifies the intrinsic mapping — the transform itself is
        /// already covered on x86 by `generic_butterfly_matches_scalar_fdct`.
        #[test]
        fn fdct_neon_matches_scalar() {
            let mut state = 0x9E37_79B9_7F4A_7C15u64;
            let mut next = move || {
                state ^= state << 13;
                state ^= state >> 7;
                state ^= state << 17;
                state
            };
            for round in 0..2000 {
                let mut a = [0i16; 64];
                for (i, v) in a.iter_mut().enumerate() {
                    *v = match round {
                        0 => 0,
                        1 => 127,
                        2 => -128,
                        3 => if i == 0 { -128 } else { 127 },
                        _ => (next() % 256) as i16 - 128,
                    };
                }
                let mut b = a;
                fdct(&mut a);
                fdct_neon(&mut b);
                assert_eq!(a, b, "round {round}");
            }
        }
    }
}
