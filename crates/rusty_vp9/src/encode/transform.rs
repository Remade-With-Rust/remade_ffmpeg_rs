//! VP9 encoder — forward transforms (Floor 1, bricks T1–T3).
//!
//! Each forward transform is the inverse of the decoder's inverse transform in
//! [`crate::transform`], reusing the same `COSPI`/`SINPI` constants and 14-bit
//! `round_shift` rounding. They are **self-verified**: a residual pushed through
//! the forward 2-D transform and then the decoder's `inverse_transform_add`
//! reconstructs the residual (within transform rounding) — no external
//! reference. The scaling is fixed by the decoder: its 2-D inverse ends in
//! `round_pow2(idct², shift_n)` with `shift = {4:4, 8:5, 16:6, 32:6}`, so the
//! forward transform must carry exactly the inverse scale (this matches the
//! libvpx forward-transform normalisation the dequant tables are calibrated to).

use crate::transform::{fdct4, TxType, COSPI};

/// 14-bit fixed-point rounding, identical to `crate::transform::round_shift`
/// (kept local so the forward butterflies read like their inverse twins).
#[inline]
fn round_shift(x: i64) -> i32 {
    ((x + (1 << 13)) >> 14) as i32
}

#[inline]
fn c(i: usize) -> i64 {
    COSPI[i]
}

// ---- size 4 ---------------------------------------------------------------

/// Forward 2-D DCT for a 4×4 residual block (row-major `n*n`), producing the
/// coefficient block in the decoder's dequantized-coefficient scale. Mirrors
/// libvpx `vpx_fdct4x4`: input ×16, an `fdct4` butterfly down columns then
/// across rows, and a final `(x + 1) >> 2`.
fn fdct4x4(residual: &[i32], out: &mut [i32]) {
    let mut inter = [0i32; 16];
    // Columns: pre-scale ×16; libvpx nudges the DC of column 0 up by 1.
    for col in 0..4 {
        let mut cin = [
            residual[col] * 16,
            residual[4 + col] * 16,
            residual[8 + col] * 16,
            residual[12 + col] * 16,
        ];
        if col == 0 && cin[0] != 0 {
            cin[0] += 1;
        }
        let mut cout = [0i32; 4];
        fdct4(&cin, &mut cout);
        for r in 0..4 {
            inter[r * 4 + col] = cout[r];
        }
    }
    // Rows: butterfly, then the final round-shift down by 2.
    for r in 0..4 {
        let rin = [
            inter[r * 4],
            inter[r * 4 + 1],
            inter[r * 4 + 2],
            inter[r * 4 + 3],
        ];
        let mut rout = [0i32; 4];
        fdct4(&rin, &mut rout);
        for c in 0..4 {
            out[r * 4 + c] = (rout[c] + 1) >> 2;
        }
    }
}

// ---- size 8 ---------------------------------------------------------------

/// Forward 8-point DCT (one dimension) — the structural inverse of
/// [`idct8`](crate::transform::idct8): an `fdct4` on the even part, a rotated
/// odd part. Outputs natural frequency order.
fn fdct8(inp: &[i32; 8], out: &mut [i32; 8]) {
    let s0 = (inp[0] + inp[7]) as i64;
    let s1 = (inp[1] + inp[6]) as i64;
    let s2 = (inp[2] + inp[5]) as i64;
    let s3 = (inp[3] + inp[4]) as i64;
    let s4 = (inp[3] - inp[4]) as i64;
    let s5 = (inp[2] - inp[5]) as i64;
    let s6 = (inp[1] - inp[6]) as i64;
    let s7 = (inp[0] - inp[7]) as i64;
    // Even part: a 4-point DCT of (s0..s3) into the even frequencies.
    let x0 = s0 + s3;
    let x1 = s1 + s2;
    let x2 = s1 - s2;
    let x3 = s0 - s3;
    out[0] = round_shift((x0 + x1) * c(16));
    out[4] = round_shift((x0 - x1) * c(16));
    out[2] = round_shift(x2 * c(24) + x3 * c(8));
    out[6] = round_shift(x3 * c(24) - x2 * c(8));
    // Odd part.
    let t2 = round_shift((s6 - s5) * c(16)) as i64;
    let t3 = round_shift((s6 + s5) * c(16)) as i64;
    let x0 = s4 + t2;
    let x1 = s4 - t2;
    let x2 = s7 - t3;
    let x3 = s7 + t3;
    out[1] = round_shift(x0 * c(28) + x3 * c(4));
    out[7] = round_shift(x3 * c(28) - x0 * c(4));
    out[5] = round_shift(x1 * c(12) + x2 * c(20));
    out[3] = round_shift(x2 * c(12) - x1 * c(20));
}

/// AVX2 `fdct8x8`: the 1-D butterfly runs 8 independent transforms at once, one
/// per i32 lane. Column pass needs NO transpose (lane j IS column j when the 8
/// rows are loaded as vectors); the row pass transposes in and back out. Uses
/// i32 `mullo` where the scalar uses i64 — bit-identical because the largest
/// intermediate product (row pass, odd part) is < 2^31 for residuals up to
/// 10-bit (±1023); 12-bit would overflow, so the caller must keep this to the
/// 8-bit encode path (debug_assert below).
///
/// # Safety
/// AVX2 must be present (caller checks).
#[cfg(target_arch = "x86_64")]
mod fdct_avx2 {
    // Split into #[target_feature] helper fns (NOT closures — closures inside a
    // target_feature fn may not inherit the feature and then spill __m256i
    // through the stack per call, which cost ~2.5× on first measurement).
    use super::c;
    use std::arch::x86_64::*;

    #[target_feature(enable = "avx2")]
    #[inline]
    unsafe fn rs(x: __m256i) -> __m256i {
        _mm256_srai_epi32::<14>(_mm256_add_epi32(x, _mm256_set1_epi32(1 << 13)))
    }

    #[target_feature(enable = "avx2")]
    #[inline]
    unsafe fn mul(x: __m256i, k: i64) -> __m256i {
        _mm256_mullo_epi32(x, _mm256_set1_epi32(k as i32))
    }

    /// The 8-lane 1-D fdct8 butterfly (mirrors the scalar `fdct8` exactly).
    #[target_feature(enable = "avx2")]
    #[inline]
    unsafe fn bfly(r: [__m256i; 8]) -> [__m256i; 8] {
        let s0 = _mm256_add_epi32(r[0], r[7]);
        let s1 = _mm256_add_epi32(r[1], r[6]);
        let s2 = _mm256_add_epi32(r[2], r[5]);
        let s3 = _mm256_add_epi32(r[3], r[4]);
        let s4 = _mm256_sub_epi32(r[3], r[4]);
        let s5 = _mm256_sub_epi32(r[2], r[5]);
        let s6 = _mm256_sub_epi32(r[1], r[6]);
        let s7 = _mm256_sub_epi32(r[0], r[7]);
        let x0 = _mm256_add_epi32(s0, s3);
        let x1 = _mm256_add_epi32(s1, s2);
        let x2 = _mm256_sub_epi32(s1, s2);
        let x3 = _mm256_sub_epi32(s0, s3);
        let o0 = rs(mul(_mm256_add_epi32(x0, x1), c(16)));
        let o4 = rs(mul(_mm256_sub_epi32(x0, x1), c(16)));
        let o2 = rs(_mm256_add_epi32(mul(x2, c(24)), mul(x3, c(8))));
        let o6 = rs(_mm256_sub_epi32(mul(x3, c(24)), mul(x2, c(8))));
        let t2 = rs(mul(_mm256_sub_epi32(s6, s5), c(16)));
        let t3 = rs(mul(_mm256_add_epi32(s6, s5), c(16)));
        let y0 = _mm256_add_epi32(s4, t2);
        let y1 = _mm256_sub_epi32(s4, t2);
        let y2 = _mm256_sub_epi32(s7, t3);
        let y3 = _mm256_add_epi32(s7, t3);
        let o1 = rs(_mm256_add_epi32(mul(y0, c(28)), mul(y3, c(4))));
        let o7 = rs(_mm256_sub_epi32(mul(y3, c(28)), mul(y0, c(4))));
        let o5 = rs(_mm256_add_epi32(mul(y1, c(12)), mul(y2, c(20))));
        let o3 = rs(_mm256_sub_epi32(mul(y2, c(12)), mul(y1, c(20))));
        [o0, o1, o2, o3, o4, o5, o6, o7]
    }

    /// 8×8 i32 transpose across the two 128-bit halves.
    #[target_feature(enable = "avx2")]
    #[inline]
    unsafe fn transpose(m: [__m256i; 8]) -> [__m256i; 8] {
        let t0 = _mm256_unpacklo_epi32(m[0], m[1]);
        let t1 = _mm256_unpackhi_epi32(m[0], m[1]);
        let t2 = _mm256_unpacklo_epi32(m[2], m[3]);
        let t3 = _mm256_unpackhi_epi32(m[2], m[3]);
        let t4 = _mm256_unpacklo_epi32(m[4], m[5]);
        let t5 = _mm256_unpackhi_epi32(m[4], m[5]);
        let t6 = _mm256_unpacklo_epi32(m[6], m[7]);
        let t7 = _mm256_unpackhi_epi32(m[6], m[7]);
        let u0 = _mm256_unpacklo_epi64(t0, t2);
        let u1 = _mm256_unpackhi_epi64(t0, t2);
        let u2 = _mm256_unpacklo_epi64(t1, t3);
        let u3 = _mm256_unpackhi_epi64(t1, t3);
        let u4 = _mm256_unpacklo_epi64(t4, t6);
        let u5 = _mm256_unpackhi_epi64(t4, t6);
        let u6 = _mm256_unpacklo_epi64(t5, t7);
        let u7 = _mm256_unpackhi_epi64(t5, t7);
        [
            _mm256_permute2x128_si256::<0x20>(u0, u4),
            _mm256_permute2x128_si256::<0x20>(u1, u5),
            _mm256_permute2x128_si256::<0x20>(u2, u6),
            _mm256_permute2x128_si256::<0x20>(u3, u7),
            _mm256_permute2x128_si256::<0x31>(u0, u4),
            _mm256_permute2x128_si256::<0x31>(u1, u5),
            _mm256_permute2x128_si256::<0x31>(u2, u6),
            _mm256_permute2x128_si256::<0x31>(u3, u7),
        ]
    }

    /// See `fdct8x8`'s docs; byte-identical to the scalar for residuals ≤ 10-bit.
    #[target_feature(enable = "avx2")]
    pub(super) unsafe fn fdct8x8(residual: &[i32], out: &mut [i32]) {
        debug_assert!(residual.iter().all(|&r| r.unsigned_abs() <= 1023));
        // Column pass: rows as vectors (lane = column), inputs pre-scaled ×4.
        let mut r: [__m256i; 8] = std::array::from_fn(|i| {
            _mm256_slli_epi32::<2>(_mm256_loadu_si256(
                residual.as_ptr().add(i * 8) as *const __m256i
            ))
        });
        r = bfly(r); // r[k] now = inter row k (coefficient k of every column)
                     // Row pass: transpose so lanes become rows, butterfly, transpose back.
        r = transpose(r);
        r = bfly(r);
        let r = transpose(r);
        // Final (x + (x<0)) >> 1 (round toward zero) and store.
        for (i, v) in r.iter().enumerate() {
            let neg = _mm256_srli_epi32::<31>(*v);
            let o = _mm256_srai_epi32::<1>(_mm256_add_epi32(*v, neg));
            _mm256_storeu_si256(out.as_mut_ptr().add(i * 8) as *mut __m256i, o);
        }
    }
}

#[cfg(target_arch = "x86_64")]
unsafe fn fdct8x8_avx2(residual: &[i32], out: &mut [i32]) {
    fdct_avx2::fdct8x8(residual, out)
}

#[cfg(target_arch = "x86_64")]
#[inline]
fn has_avx2() -> bool {
    std::is_x86_feature_detected!("avx2")
}

/// Forward 2-D DCT for an 8×8 block (libvpx `vpx_fdct8x8`): columns pre-scaled
/// ×4 through `fdct8`, then rows, with a final `>>1` rounded toward zero.
fn fdct8x8(residual: &[i32], out: &mut [i32]) {
    #[cfg(target_arch = "x86_64")]
    if has_avx2() {
        // SAFETY: AVX2 confirmed; slices are 64 i32 (asserted by the caller).
        unsafe {
            fdct8x8_avx2(residual, out);
        }
        return;
    }
    fdct8x8_scalar(residual, out)
}

/// The scalar reference — kept as the byte-identity oracle for the AVX2 twin.
fn fdct8x8_scalar(residual: &[i32], out: &mut [i32]) {
    let mut inter = [0i32; 64];
    for col in 0..8 {
        let cin: [i32; 8] = std::array::from_fn(|r| residual[r * 8 + col] * 4);
        let mut cout = [0i32; 8];
        fdct8(&cin, &mut cout);
        for r in 0..8 {
            inter[r * 8 + col] = cout[r];
        }
    }
    for r in 0..8 {
        let rin: [i32; 8] = std::array::from_fn(|cc| inter[r * 8 + cc]);
        let mut rout = [0i32; 8];
        fdct8(&rin, &mut rout);
        for cc in 0..8 {
            let x = rout[cc];
            out[r * 8 + cc] = (x + (x < 0) as i32) >> 1;
        }
    }
}

// ---- lossless Walsh-Hadamard (T2) -----------------------------------------

/// Forward 2-D Walsh-Hadamard transform for a lossless 4×4 block (libvpx
/// `vp9_fwht4x4`) — the inverse of the decoder's
/// [`inverse_wht_add`](crate::transform::inverse_wht_add). Output is `×4`
/// (`UNIT_QUANT_FACTOR`) so the decoder's `>>2` input pre-shift cancels.
pub fn fwht4x4(residual: &[i32], out: &mut [i32]) {
    let mut inter = [0i32; 16];
    for col in 0..4 {
        let (mut a, mut b, mut c, mut d) = (
            residual[col],
            residual[4 + col],
            residual[8 + col],
            residual[12 + col],
        );
        a += b;
        d -= c;
        let e = (a - d) >> 1;
        b = e - b;
        c = e - c;
        a -= c;
        d += b;
        inter[col] = a;
        inter[4 + col] = c;
        inter[8 + col] = d;
        inter[12 + col] = b;
    }
    for r in 0..4 {
        let (mut a, mut b, mut c, mut d) = (
            inter[r * 4],
            inter[r * 4 + 1],
            inter[r * 4 + 2],
            inter[r * 4 + 3],
        );
        a += b;
        d -= c;
        let e = (a - d) >> 1;
        b = e - b;
        c = e - c;
        a -= c;
        d += b;
        out[r * 4] = a * 4;
        out[r * 4 + 1] = c * 4;
        out[r * 4 + 2] = d * 4;
        out[r * 4 + 3] = b * 4;
    }
}

// ---- size 16 (fast butterfly) ---------------------------------------------

/// Use the fast `fdct16x16` butterfly for 16×16 DCT (default); `VP9_NO_FDCT16`
/// falls back to the O(n²) inverse-basis matmul (the BD-rate A/B oracle).
fn use_fdct16() -> bool {
    static F: OnceLock<bool> = OnceLock::new();
    *F.get_or_init(|| std::env::var("VP9_NO_FDCT16").is_err())
}

/// One-dimension 16-point forward DCT — the inner butterfly of libvpx
/// `vpx_fdct16x16`: an `fdct8`-shaped even part on the sums, a rotated odd part
/// on the differences. All arithmetic in i64; `round_shift` is the 14-bit DCT
/// rounding. Replaces the O(n²) inverse-basis matmul for 16×16 DCT.
fn fdct16_1d(inp: &[i64; 16], out: &mut [i64; 16]) {
    // Stage 1: sums feed the even outputs, differences feed the odd outputs.
    let mut ih = [0i64; 8];
    let mut df = [0i64; 8];
    for k in 0..8 {
        ih[k] = inp[k] + inp[15 - k];
        df[k] = inp[7 - k] - inp[8 + k];
    }
    // Even part — an fdct8 of `ih` into the even frequencies.
    {
        let (a0, a1, a2, a3) = (ih[0] + ih[7], ih[1] + ih[6], ih[2] + ih[5], ih[3] + ih[4]);
        let (a4, a5, a6, a7) = (ih[3] - ih[4], ih[2] - ih[5], ih[1] - ih[6], ih[0] - ih[7]);
        let x0 = a0 + a3;
        let x1 = a1 + a2;
        let x2 = a1 - a2;
        let x3 = a0 - a3;
        out[0] = round_shift((x0 + x1) * c(16)) as i64;
        out[8] = round_shift((x0 - x1) * c(16)) as i64;
        out[4] = round_shift(x3 * c(8) + x2 * c(24)) as i64;
        out[12] = round_shift(x3 * c(24) - x2 * c(8)) as i64;
        let t2 = round_shift((a6 - a5) * c(16)) as i64;
        let t3 = round_shift((a6 + a5) * c(16)) as i64;
        let y0 = a4 + t2;
        let y1 = a4 - t2;
        let y2 = a7 - t3;
        let y3 = a7 + t3;
        out[2] = round_shift(y0 * c(28) + y3 * c(4)) as i64;
        out[14] = round_shift(y3 * c(28) - y0 * c(4)) as i64;
        out[10] = round_shift(y1 * c(12) + y2 * c(20)) as i64;
        out[6] = round_shift(y2 * c(12) - y1 * c(20)) as i64;
    }
    // Odd part — rotations on the differences `df` into the odd frequencies.
    {
        let p2 = round_shift((df[5] - df[2]) * c(16)) as i64;
        let p3 = round_shift((df[4] - df[3]) * c(16)) as i64;
        let p4 = round_shift((df[4] + df[3]) * c(16)) as i64;
        let p5 = round_shift((df[5] + df[2]) * c(16)) as i64;
        let q0 = df[0] + p3;
        let q1 = df[1] + p2;
        let q2 = df[1] - p2;
        let q3 = df[0] - p3;
        let q4 = df[7] - p4;
        let q5 = df[6] - p5;
        let q6 = df[6] + p5;
        let q7 = df[7] + p4;
        let r1 = round_shift(q1 * (-c(8)) + q6 * c(24)) as i64;
        let r2 = round_shift(q2 * c(24) + q5 * c(8)) as i64;
        let r5 = round_shift(q2 * c(8) - q5 * c(24)) as i64;
        let r6 = round_shift(q1 * c(24) + q6 * c(8)) as i64;
        let u0 = q0 + r1;
        let u1 = q0 - r1;
        let u2 = q3 + r2;
        let u3 = q3 - r2;
        let u4 = q4 - r5;
        let u5 = q4 + r5;
        let u6 = q7 - r6;
        let u7 = q7 + r6;
        out[1] = round_shift(u0 * c(30) + u7 * c(2)) as i64;
        out[9] = round_shift(u1 * c(14) + u6 * c(18)) as i64;
        out[5] = round_shift(u2 * c(22) + u5 * c(10)) as i64;
        out[13] = round_shift(u3 * c(6) + u4 * c(26)) as i64;
        out[3] = round_shift(u3 * (-c(26)) + u4 * c(6)) as i64;
        out[11] = round_shift(u2 * (-c(10)) + u5 * c(22)) as i64;
        out[7] = round_shift(u1 * (-c(18)) + u6 * c(14)) as i64;
        out[15] = round_shift(u0 * (-c(2)) + u7 * c(30)) as i64;
    }
}

/// Forward 2-D 16×16 DCT (libvpx `vpx_fdct16x16`): a column pass (×4 pre-scale)
/// then a row pass (`(x+1)>>2` pre-scale). Output is in the decoder's
/// dequantized-coefficient scale — the same domain `fdct4x4`/`fdct8x8` produce,
/// so the decoder's `idct16` inverts it (gated by the round-trip test).
fn fdct16x16(residual: &[i32], out: &mut [i32], inter: &mut [i64]) {
    // `inter` is caller scratch (>= 256): pass 0 writes all 256 entries before
    // pass 1 reads any, so stale contents are unobservable.
    let inter = &mut inter[..256];
    // Pass 0: columns, input ×4, stored transposed (each column's 16 freqs run).
    for col in 0..16 {
        let mut cin = [0i64; 16];
        for k in 0..16 {
            cin[k] = residual[k * 16 + col] as i64 * 4;
        }
        let mut cout = [0i64; 16];
        fdct16_1d(&cin, &mut cout);
        for f in 0..16 {
            inter[col * 16 + f] = cout[f];
        }
    }
    // Pass 1: rows, input (x+1)>>2.
    for i in 0..16 {
        let mut rin = [0i64; 16];
        for k in 0..16 {
            rin[k] = (inter[k * 16 + i] + 1) >> 2;
        }
        let mut rout = [0i64; 16];
        fdct16_1d(&rin, &mut rout);
        for f in 0..16 {
            out[i * 16 + f] = rout[f] as i32;
        }
    }
}

// ---- size 32 (fast butterfly) ---------------------------------------------

/// Use the fast `fdct32x32` butterfly for 32×32 DCT (default); `VP9_NO_FDCT32`
/// falls back to the O(n²) inverse-basis matmul (the BD-rate A/B oracle).
fn use_fdct32() -> bool {
    static F: OnceLock<bool> = OnceLock::new();
    *F.get_or_init(|| std::env::var("VP9_NO_FDCT32").is_err())
}

/// One-dimension 32-point forward DCT — libvpx `vpx_fdct32` with `round == 0`
/// (the RDO path; the `half_round_shift` branch is unused). Eight butterfly
/// stages alternating between the `step` scratch and `out`; `round_shift` is the
/// 14-bit DCT rounding (`dct_32_round`). Replaces the O(n²) matmul for 32×32.
fn fdct32_1d(input: &[i64; 32], out: &mut [i64; 32]) {
    let rs = |x: i64| round_shift(x) as i64;
    let mut step = [0i64; 32];
    // Stage 1
    for k in 0..16 {
        step[k] = input[k] + input[31 - k];
    }
    for k in 16..32 {
        step[k] = -input[k] + input[31 - k];
    }
    // Stage 2
    for k in 0..8 {
        out[k] = step[k] + step[15 - k];
    }
    for k in 8..16 {
        out[k] = -step[k] + step[15 - k];
    }
    out[16] = step[16];
    out[17] = step[17];
    out[18] = step[18];
    out[19] = step[19];
    out[20] = rs((-step[20] + step[27]) * c(16));
    out[21] = rs((-step[21] + step[26]) * c(16));
    out[22] = rs((-step[22] + step[25]) * c(16));
    out[23] = rs((-step[23] + step[24]) * c(16));
    out[24] = rs((step[24] + step[23]) * c(16));
    out[25] = rs((step[25] + step[22]) * c(16));
    out[26] = rs((step[26] + step[21]) * c(16));
    out[27] = rs((step[27] + step[20]) * c(16));
    out[28] = step[28];
    out[29] = step[29];
    out[30] = step[30];
    out[31] = step[31];
    // Stage 3
    step[0] = out[0] + out[7];
    step[1] = out[1] + out[6];
    step[2] = out[2] + out[5];
    step[3] = out[3] + out[4];
    step[4] = -out[4] + out[3];
    step[5] = -out[5] + out[2];
    step[6] = -out[6] + out[1];
    step[7] = -out[7] + out[0];
    step[8] = out[8];
    step[9] = out[9];
    step[10] = rs((-out[10] + out[13]) * c(16));
    step[11] = rs((-out[11] + out[12]) * c(16));
    step[12] = rs((out[12] + out[11]) * c(16));
    step[13] = rs((out[13] + out[10]) * c(16));
    step[14] = out[14];
    step[15] = out[15];
    step[16] = out[16] + out[23];
    step[17] = out[17] + out[22];
    step[18] = out[18] + out[21];
    step[19] = out[19] + out[20];
    step[20] = -out[20] + out[19];
    step[21] = -out[21] + out[18];
    step[22] = -out[22] + out[17];
    step[23] = -out[23] + out[16];
    step[24] = -out[24] + out[31];
    step[25] = -out[25] + out[30];
    step[26] = -out[26] + out[29];
    step[27] = -out[27] + out[28];
    step[28] = out[28] + out[27];
    step[29] = out[29] + out[26];
    step[30] = out[30] + out[25];
    step[31] = out[31] + out[24];
    // Stage 4
    out[0] = step[0] + step[3];
    out[1] = step[1] + step[2];
    out[2] = -step[2] + step[1];
    out[3] = -step[3] + step[0];
    out[4] = step[4];
    out[5] = rs((-step[5] + step[6]) * c(16));
    out[6] = rs((step[6] + step[5]) * c(16));
    out[7] = step[7];
    out[8] = step[8] + step[11];
    out[9] = step[9] + step[10];
    out[10] = -step[10] + step[9];
    out[11] = -step[11] + step[8];
    out[12] = -step[12] + step[15];
    out[13] = -step[13] + step[14];
    out[14] = step[14] + step[13];
    out[15] = step[15] + step[12];
    out[16] = step[16];
    out[17] = step[17];
    out[18] = rs(step[18] * -c(8) + step[29] * c(24));
    out[19] = rs(step[19] * -c(8) + step[28] * c(24));
    out[20] = rs(step[20] * -c(24) + step[27] * -c(8));
    out[21] = rs(step[21] * -c(24) + step[26] * -c(8));
    out[22] = step[22];
    out[23] = step[23];
    out[24] = step[24];
    out[25] = step[25];
    out[26] = rs(step[26] * c(24) + step[21] * -c(8));
    out[27] = rs(step[27] * c(24) + step[20] * -c(8));
    out[28] = rs(step[28] * c(8) + step[19] * c(24));
    out[29] = rs(step[29] * c(8) + step[18] * c(24));
    out[30] = step[30];
    out[31] = step[31];
    // Stage 5
    step[0] = rs((out[0] + out[1]) * c(16));
    step[1] = rs((-out[1] + out[0]) * c(16));
    step[2] = rs(out[2] * c(24) + out[3] * c(8));
    step[3] = rs(out[3] * c(24) - out[2] * c(8));
    step[4] = out[4] + out[5];
    step[5] = -out[5] + out[4];
    step[6] = -out[6] + out[7];
    step[7] = out[7] + out[6];
    step[8] = out[8];
    step[9] = rs(out[9] * -c(8) + out[14] * c(24));
    step[10] = rs(out[10] * -c(24) + out[13] * -c(8));
    step[11] = out[11];
    step[12] = out[12];
    step[13] = rs(out[13] * c(24) + out[10] * -c(8));
    step[14] = rs(out[14] * c(8) + out[9] * c(24));
    step[15] = out[15];
    step[16] = out[16] + out[19];
    step[17] = out[17] + out[18];
    step[18] = -out[18] + out[17];
    step[19] = -out[19] + out[16];
    step[20] = -out[20] + out[23];
    step[21] = -out[21] + out[22];
    step[22] = out[22] + out[21];
    step[23] = out[23] + out[20];
    step[24] = out[24] + out[27];
    step[25] = out[25] + out[26];
    step[26] = -out[26] + out[25];
    step[27] = -out[27] + out[24];
    step[28] = -out[28] + out[31];
    step[29] = -out[29] + out[30];
    step[30] = out[30] + out[29];
    step[31] = out[31] + out[28];
    // Stage 6
    out[0] = step[0];
    out[1] = step[1];
    out[2] = step[2];
    out[3] = step[3];
    out[4] = rs(step[4] * c(28) + step[7] * c(4));
    out[5] = rs(step[5] * c(12) + step[6] * c(20));
    out[6] = rs(step[6] * c(12) + step[5] * -c(20));
    out[7] = rs(step[7] * c(28) + step[4] * -c(4));
    out[8] = step[8] + step[9];
    out[9] = -step[9] + step[8];
    out[10] = -step[10] + step[11];
    out[11] = step[11] + step[10];
    out[12] = step[12] + step[13];
    out[13] = -step[13] + step[12];
    out[14] = -step[14] + step[15];
    out[15] = step[15] + step[14];
    out[16] = step[16];
    out[17] = rs(step[17] * -c(4) + step[30] * c(28));
    out[18] = rs(step[18] * -c(28) + step[29] * -c(4));
    out[19] = step[19];
    out[20] = step[20];
    out[21] = rs(step[21] * -c(20) + step[26] * c(12));
    out[22] = rs(step[22] * -c(12) + step[25] * -c(20));
    out[23] = step[23];
    out[24] = step[24];
    out[25] = rs(step[25] * c(12) + step[22] * -c(20));
    out[26] = rs(step[26] * c(20) + step[21] * c(12));
    out[27] = step[27];
    out[28] = step[28];
    out[29] = rs(step[29] * c(28) + step[18] * -c(4));
    out[30] = rs(step[30] * c(4) + step[17] * c(28));
    out[31] = step[31];
    // Stage 7
    step[0] = out[0];
    step[1] = out[1];
    step[2] = out[2];
    step[3] = out[3];
    step[4] = out[4];
    step[5] = out[5];
    step[6] = out[6];
    step[7] = out[7];
    step[8] = rs(out[8] * c(30) + out[15] * c(2));
    step[9] = rs(out[9] * c(14) + out[14] * c(18));
    step[10] = rs(out[10] * c(22) + out[13] * c(10));
    step[11] = rs(out[11] * c(6) + out[12] * c(26));
    step[12] = rs(out[12] * c(6) + out[11] * -c(26));
    step[13] = rs(out[13] * c(22) + out[10] * -c(10));
    step[14] = rs(out[14] * c(14) + out[9] * -c(18));
    step[15] = rs(out[15] * c(30) + out[8] * -c(2));
    step[16] = out[16] + out[17];
    step[17] = -out[17] + out[16];
    step[18] = -out[18] + out[19];
    step[19] = out[19] + out[18];
    step[20] = out[20] + out[21];
    step[21] = -out[21] + out[20];
    step[22] = -out[22] + out[23];
    step[23] = out[23] + out[22];
    step[24] = out[24] + out[25];
    step[25] = -out[25] + out[24];
    step[26] = -out[26] + out[27];
    step[27] = out[27] + out[26];
    step[28] = out[28] + out[29];
    step[29] = -out[29] + out[28];
    step[30] = -out[30] + out[31];
    step[31] = out[31] + out[30];
    // Final stage — outputs bit-reversed.
    out[0] = step[0];
    out[16] = step[1];
    out[8] = step[2];
    out[24] = step[3];
    out[4] = step[4];
    out[20] = step[5];
    out[12] = step[6];
    out[28] = step[7];
    out[2] = step[8];
    out[18] = step[9];
    out[10] = step[10];
    out[26] = step[11];
    out[6] = step[12];
    out[22] = step[13];
    out[14] = step[14];
    out[30] = step[15];
    out[1] = rs(step[16] * c(31) + step[31] * c(1));
    out[17] = rs(step[17] * c(15) + step[30] * c(17));
    out[9] = rs(step[18] * c(23) + step[29] * c(9));
    out[25] = rs(step[19] * c(7) + step[28] * c(25));
    out[5] = rs(step[20] * c(27) + step[27] * c(5));
    out[21] = rs(step[21] * c(11) + step[26] * c(21));
    out[13] = rs(step[22] * c(19) + step[25] * c(13));
    out[29] = rs(step[23] * c(3) + step[24] * c(29));
    out[3] = rs(step[24] * c(3) + step[23] * -c(29));
    out[19] = rs(step[25] * c(19) + step[22] * -c(13));
    out[11] = rs(step[26] * c(11) + step[21] * -c(21));
    out[27] = rs(step[27] * c(27) + step[20] * -c(5));
    out[7] = rs(step[28] * c(7) + step[19] * -c(25));
    out[23] = rs(step[29] * c(23) + step[18] * -c(9));
    out[15] = rs(step[30] * c(15) + step[17] * -c(17));
    out[31] = rs(step[31] * c(31) + step[16] * -c(1));
}

/// Forward 2-D 32×32 DCT (libvpx `vpx_fdct32x32`): both passes pre-scale ×4 /
/// asymmetric `(x+1+sign)>>2` round to keep intermediates in range. Output in
/// the decoder's dequantized-coefficient scale (gated by the round-trip test).
fn fdct32x32(residual: &[i32], out: &mut [i32], inter: &mut [i64]) {
    // `inter` is caller scratch (>= 1024), fully written by the column pass.
    let inter = &mut inter[..1024];
    // Columns
    for i in 0..32 {
        let mut tin = [0i64; 32];
        for j in 0..32 {
            tin[j] = residual[j * 32 + i] as i64 * 4;
        }
        let mut tout = [0i64; 32];
        fdct32_1d(&tin, &mut tout);
        for j in 0..32 {
            inter[j * 32 + i] = (tout[j] + 1 + (tout[j] > 0) as i64) >> 2;
        }
    }
    // Rows
    for i in 0..32 {
        let mut tin = [0i64; 32];
        for j in 0..32 {
            tin[j] = inter[j + i * 32];
        }
        let mut tout = [0i64; 32];
        fdct32_1d(&tin, &mut tout);
        for j in 0..32 {
            out[j + i * 32] = ((tout[j] + 1 + (tout[j] < 0) as i64) >> 2) as i32;
        }
    }
}

// ---- sizes 16/32 + ADST: integer transpose of the decoder's inverse --------
//
// For the larger DCTs and every ADST, the forward transform is built directly
// from the decoder's own inverse: `basis[k] = inv_1d(e_k << 14)`, so the forward
// is `out[k] = Σ_i in[i]·basis[k][i]` — the integer transpose of the inverse
// matrix (pure reuse, no fabricated constants). Both `idct` and `iadst` have the
// same 1-D energy gain N/2 (the decoder's orthogonality tests prove it), so a
// single per-size calibration shift makes the 2-D round-trip exact regardless of
// the DCT/ADST mix. `O(n²)` and correctness-first; replacing with fast integer
// butterflies is a Roof optimisation (the round-trip gate guards any swap).

use std::sync::OnceLock;

use crate::transform::{iadst16, iadst4, iadst8, idct16, idct32, idct4, idct8};

/// Per-size calibration shift: `26 - shift_n + 2·log2(n)`, where the decoder's
/// 2-D inverse ends in `round_pow2(·, shift_n)`. Derived from the √(N/2) gain and
/// confirmed by the round-trip gate.
fn calib_shift(n: usize) -> u32 {
    match n {
        4 => 26,
        8 => 27,
        16 => 28,
        32 => 30,
        _ => unreachable!(),
    }
}

/// One-dimension inverse transform dispatch onto the decoder's functions.
fn inv_1d(inp: &[i32], out: &mut [i32], adst: bool) {
    match (inp.len(), adst) {
        (4, false) => idct4(inp.try_into().unwrap(), (&mut out[..4]).try_into().unwrap()),
        (8, false) => idct8(inp.try_into().unwrap(), (&mut out[..8]).try_into().unwrap()),
        (16, false) => idct16(
            inp.try_into().unwrap(),
            (&mut out[..16]).try_into().unwrap(),
        ),
        (32, false) => idct32(
            inp.try_into().unwrap(),
            (&mut out[..32]).try_into().unwrap(),
        ),
        (4, true) => iadst4(inp.try_into().unwrap(), (&mut out[..4]).try_into().unwrap()),
        (8, true) => iadst8(inp.try_into().unwrap(), (&mut out[..8]).try_into().unwrap()),
        (16, true) => iadst16(
            inp.try_into().unwrap(),
            (&mut out[..16]).try_into().unwrap(),
        ),
        _ => unreachable!("no inverse 1-D for {} adst={adst}", inp.len()),
    }
}

/// Build the inverse-transform basis matrix: `basis[k][i] = inv_1d(e_k << 14)[i]`.
fn build_basis(n: usize, adst: bool) -> Vec<Vec<i64>> {
    (0..n)
        .map(|k| {
            let mut inp = vec![0i32; n];
            inp[k] = 1 << 14;
            let mut out = vec![0i32; n];
            inv_1d(&inp, &mut out, adst);
            out.iter().map(|&v| v as i64).collect()
        })
        .collect()
}

/// Tables for the matrix forward transform of one `(n, adst)` pair.
struct MatBasis {
    /// `b[k * n + i] = basis[k][i]` — the inverse basis, row `k` contiguous.
    b: Vec<i64>,
    /// Row-pass pair table for `pmaddwd`: for input pair `j` and output `k`,
    /// `rp[(j * n + k) * 2 + t] = basis[k][2 * j + t]` as `i16`.
    rp: Vec<i16>,
    /// Largest `|residual|` for which the vector row pass is exact: operands fit
    /// `i16` and no partial sum of a row's products can leave `i32`. Derived
    /// from the table itself (`i32::MAX / max_k Σ_i |b[k][i]|`); 0 disables it.
    lim: i32,
}

fn build_mat_basis(n: usize, adst: bool) -> MatBasis {
    let b: Vec<i64> = build_basis(n, adst).into_iter().flatten().collect();
    let fits = b.iter().all(|v| v.unsigned_abs() <= i16::MAX as u64);
    let l1 = b
        .chunks_exact(n)
        .map(|row| row.iter().map(|v| v.abs()).sum::<i64>())
        .max()
        .unwrap_or(1)
        .max(1);
    let lim = if fits {
        (i32::MAX as i64 / l1).min(i16::MAX as i64) as i32
    } else {
        0
    };
    let mut rp = vec![0i16; n * n];
    if fits {
        for j in 0..n / 2 {
            for k in 0..n {
                for t in 0..2 {
                    rp[(j * n + k) * 2 + t] = b[k * n + 2 * j + t] as i16;
                }
            }
        }
    }
    MatBasis { b, rp, lim }
}

/// Cached matrix-transform tables for `(n, adst)`.
///
/// One table set behind one `OnceLock`, each matrix a single contiguous slice.
/// The previous shape (a `Vec<Vec<i64>>` per pair, each behind its own lock)
/// cost two lock probes per call and a pointer chase plus a bounds check per
/// row of every dot product.
fn mat_basis(n: usize, adst: bool) -> &'static MatBasis {
    static T: OnceLock<[MatBasis; 8]> = OnceLock::new();
    let t = T.get_or_init(|| {
        std::array::from_fn(|idx| {
            let (n, adst) = (4usize << (idx >> 1), idx & 1 == 1);
            if n == 32 && adst {
                // VP9 has no 32-point ADST.
                return MatBasis {
                    b: Vec::new(),
                    rp: Vec::new(),
                    lim: 0,
                };
            }
            build_mat_basis(n, adst)
        })
    });
    let m = &t[((n.trailing_zeros() as usize - 2) << 1) | adst as usize];
    assert!(m.b.len() == n * n, "no basis for {n} adst={adst}");
    m
}

/// Forward 2-D transform via the integer inverse-basis transpose. `row_adst` /
/// `col_adst` select the per-dimension transform exactly as the decoder's
/// `inverse_transform_add_rows` interprets `tx_type`.
///
/// The result is the exact integer product `B_col · R · B_rowᵀ` with a single
/// final round-shift, so any evaluation order gives the same output.
fn forward_2d_matrix(
    residual: &[i32],
    n: usize,
    row_adst: bool,
    col_adst: bool,
    out: &mut [i32],
    tmp: &mut [i64],
) {
    let rm = mat_basis(n, row_adst);
    let cm = mat_basis(n, col_adst);
    #[cfg(target_arch = "x86_64")]
    if n <= 16 && rm.lim > 0 && has_avx2() {
        // SAFETY: AVX2 confirmed; the slices are length-checked inside.
        let done = unsafe {
            match n {
                4 => matrix_avx2::forward::<4>(residual, rm, &cm.b, out, tmp),
                8 => matrix_avx2::forward::<8>(residual, rm, &cm.b, out, tmp),
                _ => matrix_avx2::forward::<16>(residual, rm, &cm.b, out, tmp),
            }
        };
        if done {
            return;
        }
    }
    forward_2d_matrix_scalar(residual, n, &rm.b, &cm.b, out, tmp);
}

fn forward_2d_matrix_scalar(
    residual: &[i32],
    n: usize,
    rb: &[i64],
    cb: &[i64],
    out: &mut [i32],
    tmp: &mut [i64],
) {
    match n {
        4 => matrix_fwd::<4>(residual, rb, cb, out, tmp),
        8 => matrix_fwd::<8>(residual, rb, cb, out, tmp),
        16 => matrix_fwd::<16>(residual, rb, cb, out, tmp),
        32 => matrix_fwd::<32>(residual, rb, cb, out, tmp),
        _ => unreachable!("no matrix transform for n={n}"),
    }
}

/// The scalar matrix transform for one size — the exact-integer reference.
///
/// `tmp` is caller scratch: a `[0i64; 32 * 32]` local here was an 8 KiB
/// zero-fill on every ADST tx block, of which a 4x4 uses 128 bytes. The row
/// pass writes all of `[..N * N]` before the column pass reads it.
#[inline(always)]
fn matrix_fwd<const N: usize>(
    residual: &[i32],
    rb: &[i64],
    cb: &[i64],
    out: &mut [i32],
    tmp: &mut [i64],
) {
    let (residual, rb) = (&residual[..N * N], &rb[..N * N]);
    let tmp = &mut tmp[..N * N];
    // Row pass: tmp[r][k] = Σ_i residual[r][i] · rb[k][i].
    for (rrow, trow) in residual.chunks_exact(N).zip(tmp.chunks_exact_mut(N)) {
        for (t, bk) in trow.iter_mut().zip(rb.chunks_exact(N)) {
            let mut acc = 0i64;
            for (&x, &b) in rrow.iter().zip(bk) {
                acc += x as i64 * b;
            }
            *t = acc;
        }
    }
    matrix_cols::<N>(tmp, cb, out);
}

/// Column pass + calibration round-shift of the matrix transform:
/// `out[kr][kc] = (Σ_r tmp[r][kc] · cb[kr][r] + round) >> sh`, accumulated a
/// whole output row at a time so `tmp` is read along its rows.
#[inline(always)]
fn matrix_cols<const N: usize>(tmp: &[i64], cb: &[i64], out: &mut [i32]) {
    let (tmp, cb, out) = (&tmp[..N * N], &cb[..N * N], &mut out[..N * N]);
    let sh = calib_shift(N);
    let round = 1i64 << (sh - 1);
    for (orow, bk) in out.chunks_exact_mut(N).zip(cb.chunks_exact(N)) {
        let mut acc = [0i64; N];
        for (trow, &b) in tmp.chunks_exact(N).zip(bk) {
            for (a, &t) in acc.iter_mut().zip(trow) {
                *a += t * b;
            }
        }
        for (o, a) in orow.iter_mut().zip(acc) {
            *o = ((a + round) >> sh) as i32;
        }
    }
}

/// Stage switch for the vector column pass (see [`matrix_avx2`]).
#[cfg(target_arch = "x86_64")]
const MATRIX_AVX2_COLS: bool = false;

/// AVX2 twin of [`matrix_fwd`] for `N` in `{4, 8, 16}`. Bit-identical: the row
/// pass is exact in `i32` under the table-derived `lim` guard, the column pass
/// is the same exact `i64` sum, and the final shift keeps the low 32 bits.
#[cfg(target_arch = "x86_64")]
mod matrix_avx2 {
    use super::{calib_shift, MatBasis};
    use std::arch::x86_64::*;

    /// Returns `false` when a residual exceeds the row-pass bound (`out` is
    /// then unspecified); the caller runs the scalar path instead.
    ///
    /// # Safety
    /// AVX2 must be available. `N` must be 4, 8 or 16.
    #[target_feature(enable = "avx2")]
    pub(super) unsafe fn forward<const N: usize>(
        residual: &[i32],
        rm: &MatBasis,
        cb: &[i64],
        out: &mut [i32],
        tmp: &mut [i64],
    ) -> bool {
        let (residual, cb) = (&residual[..N * N], &cb[..N * N]);
        let (out, tmp) = (&mut out[..N * N], &mut tmp[..N * N]);
        let rp = &rm.rp[..N * N];
        if !rows::<N>(residual, rp, rm.lim, tmp) {
            return false;
        }
        if super::MATRIX_AVX2_COLS {
            cols::<N>(tmp, cb, out);
        } else {
            super::matrix_cols::<N>(tmp, cb, out);
        }
        true
    }

    /// Row pass: `tmp[r][k] = Σ_i residual[r][i] · basis[k][i]`, two taps per
    /// `pmaddwd` with the output index `k` in the lanes. Results are stored
    /// sign-extended to `i64` for the column pass.
    #[target_feature(enable = "avx2")]
    #[inline]
    unsafe fn rows<const N: usize>(
        residual: &[i32],
        rp: &[i16],
        lim: i32,
        tmp: &mut [i64],
    ) -> bool {
        let res = residual.as_ptr();
        let t = tmp.as_mut_ptr();
        let p = rp.as_ptr();
        if N == 4 {
            let limv = _mm_set1_epi32(lim);
            let t0 = _mm_loadu_si128(p as *const __m128i);
            let t1 = _mm_loadu_si128(p.add(8) as *const __m128i);
            let mut over = _mm_setzero_si128();
            for r in 0..4 {
                let x = _mm_loadu_si128(res.add(4 * r) as *const __m128i);
                over = _mm_or_si128(over, _mm_cmpgt_epi32(_mm_abs_epi32(x), limv));
                let pk = _mm_packs_epi32(x, x);
                let acc = _mm_add_epi32(
                    _mm_madd_epi16(_mm_shuffle_epi32::<0x00>(pk), t0),
                    _mm_madd_epi16(_mm_shuffle_epi32::<0x55>(pk), t1),
                );
                _mm256_storeu_si256(t.add(4 * r) as *mut __m256i, _mm256_cvtepi32_epi64(acc));
            }
            return _mm_testz_si128(over, over) == 1;
        }
        let limv = _mm256_set1_epi32(lim);
        let mut over = _mm256_setzero_si256();
        for r in 0..N {
            // Pack the row to i16 pairs: lane j of `pk` holds inputs (2j, 2j+1).
            let pk = if N == 8 {
                let x = _mm256_loadu_si256(res.add(8 * r) as *const __m256i);
                over = _mm256_or_si256(over, _mm256_cmpgt_epi32(_mm256_abs_epi32(x), limv));
                _mm256_castsi128_si256(_mm_packs_epi32(
                    _mm256_castsi256_si128(x),
                    _mm256_extracti128_si256::<1>(x),
                ))
            } else {
                let x0 = _mm256_loadu_si256(res.add(16 * r) as *const __m256i);
                let x1 = _mm256_loadu_si256(res.add(16 * r + 8) as *const __m256i);
                over = _mm256_or_si256(over, _mm256_cmpgt_epi32(_mm256_abs_epi32(x0), limv));
                over = _mm256_or_si256(over, _mm256_cmpgt_epi32(_mm256_abs_epi32(x1), limv));
                _mm256_permute4x64_epi64::<0xD8>(_mm256_packs_epi32(x0, x1))
            };
            let mut acc = [_mm256_setzero_si256(); 2];
            for j in 0..N / 2 {
                let bj = _mm256_permutevar8x32_epi32(pk, _mm256_set1_epi32(j as i32));
                for (v, a) in acc.iter_mut().enumerate().take(N / 8) {
                    let tab = _mm256_loadu_si256(p.add((j * N + 8 * v) * 2) as *const __m256i);
                    *a = _mm256_add_epi32(*a, _mm256_madd_epi16(bj, tab));
                }
            }
            for (v, a) in acc.iter().enumerate().take(N / 8) {
                let d = t.add(r * N + 8 * v);
                _mm256_storeu_si256(
                    d as *mut __m256i,
                    _mm256_cvtepi32_epi64(_mm256_castsi256_si128(*a)),
                );
                _mm256_storeu_si256(
                    d.add(4) as *mut __m256i,
                    _mm256_cvtepi32_epi64(_mm256_extracti128_si256::<1>(*a)),
                );
            }
        }
        _mm256_testz_si256(over, over) == 1
    }

    /// Column pass: `out[kr][kc] = (Σ_r tmp[r][kc] · cb[kr][r] + round) >> sh`.
    /// `pmuldq` gives the exact signed 64-bit products (both operands fit
    /// `i32`); the arithmetic shift is taken as a logical one and truncated,
    /// which agrees in the low 32 bits for any shift up to 32.
    #[target_feature(enable = "avx2")]
    #[inline]
    unsafe fn cols<const N: usize>(tmp: &[i64], cb: &[i64], out: &mut [i32]) {
        let t = tmp.as_ptr();
        let o = out.as_mut_ptr();
        let sh = calib_shift(N);
        let round = _mm256_set1_epi64x(1i64 << (sh - 1));
        let shv = _mm_cvtsi32_si128(sh as i32);
        let low = _mm256_setr_epi32(0, 2, 4, 6, 0, 2, 4, 6);
        for kr in 0..N {
            let mut acc = [_mm256_setzero_si256(); 4];
            for r in 0..N {
                let b = _mm256_set1_epi64x(*cb.get_unchecked(kr * N + r));
                for (c, a) in acc.iter_mut().enumerate().take(N / 4) {
                    let x = _mm256_loadu_si256(t.add(r * N + 4 * c) as *const __m256i);
                    *a = _mm256_add_epi64(*a, _mm256_mul_epi32(x, b));
                }
            }
            for (c, a) in acc.iter().enumerate().take(N / 4) {
                let v = _mm256_srl_epi64(_mm256_add_epi64(*a, round), shv);
                let v = _mm256_permutevar8x32_epi32(v, low);
                _mm_storeu_si128(
                    o.add(kr * N + 4 * c) as *mut __m128i,
                    _mm256_castsi256_si128(v),
                );
            }
        }
    }
}

/// Map `TxType` to `(row_adst, col_adst)`, matching the decoder.
fn tx_dirs(tx_type: TxType) -> (bool, bool) {
    match tx_type {
        TxType::DctDct => (false, false),
        TxType::AdstDct => (false, true),
        TxType::DctAdst => (true, false),
        TxType::AdstAdst => (true, true),
    }
}

// ---- 2-D dispatch (T3) ----------------------------------------------------

/// Forward 2-D transform of an `n×n` residual block (row-major) into `out`
/// (row-major coefficients). `tx_type` selects DCT/ADST per dimension exactly
/// as the decoder's `inverse_transform_add` interprets it. The output is in the
/// decoder's dequantized-coefficient domain (divide by the quant step to get
/// levels). Only DCT_DCT is wired so far; ADST and sizes 8/16/32 land next.
pub fn forward_transform(residual: &[i32], n: usize, tx_type: TxType, out: &mut [i32]) {
    let mut tmp = [0i64; 32 * 32];
    forward_transform_scratch(residual, n, tx_type, out, &mut tmp);
}

/// [`forward_transform`] with caller-owned intermediate scratch (`tmp.len() >=
/// 1024`). The encoder's per-tx-block path uses this so the 16x16 / 32x32 / ADST
/// intermediates are not a 2-8 KiB zero-fill per call; contents of `tmp` on entry
/// are irrelevant (every pass writes what the next reads) and the output is
/// identical to [`forward_transform`].
pub fn forward_transform_scratch(
    residual: &[i32],
    n: usize,
    tx_type: TxType,
    out: &mut [i32],
    tmp: &mut [i64],
) {
    debug_assert_eq!(residual.len(), n * n);
    debug_assert!(out.len() >= n * n);
    debug_assert!(tmp.len() >= n * n);
    match (n, tx_type) {
        // Exact integer butterflies for the DCTs (fast; O(n log n)).
        (4, TxType::DctDct) => fdct4x4(residual, out),
        (8, TxType::DctDct) => fdct8x8(residual, out),
        (16, TxType::DctDct) if use_fdct16() => fdct16x16(residual, out, tmp),
        (32, TxType::DctDct) if use_fdct32() => fdct32x32(residual, out, tmp),
        // Everything else (all ADST — and 16/32 DCT under VP9_NO_FDCT16/32)
        // via the inverse-basis transpose.
        _ => {
            let (row_adst, col_adst) = tx_dirs(tx_type);
            forward_2d_matrix(residual, n, row_adst, col_adst, out, tmp);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::transform::inverse_transform_add;

    fn xs(s: &mut u64) -> u64 {
        *s ^= *s << 13;
        *s ^= *s >> 7;
        *s ^= *s << 17;
        *s
    }

    /// Push a residual through the forward 2-D transform and straight back
    /// through the decoder's inverse (quant step = 1, i.e. coeffs == dqcoeff);
    /// the reconstruction must match the residual within transform rounding.
    fn assert_transform_roundtrips(n: usize, tx: TxType, max_err: i32) {
        let mut s = 0x9e37_79b9_7f4a_7c15u64 ^ (n as u64);
        let base = 128i32; // mid-level prediction; residual sits around it
        for _ in 0..200 {
            let residual: Vec<i32> = (0..n * n)
                .map(|_| (xs(&mut s) % 121) as i32 - 60) // [-60, 60]
                .collect();
            let mut coeffs = vec![0i32; n * n];
            forward_transform(&residual, n, tx, &mut coeffs);

            // Reconstruct: dest = prediction (flat `base`), add inverse(coeffs).
            let mut dest = vec![base as u16; n * n];
            inverse_transform_add(&coeffs, n, tx, &mut dest, n, 4095);

            for i in 0..n * n {
                let got = dest[i] as i32 - base;
                assert!(
                    (got - residual[i]).abs() <= max_err,
                    "{n}x{n} {tx:?} pos {i}: recon {got} vs residual {} (coeff {})",
                    residual[i],
                    coeffs[i]
                );
            }
        }
    }

    #[test]
    fn fdct4x4_roundtrips_through_decoder() {
        // The transform pair is exact up to rounding: a couple of units max.
        assert_transform_roundtrips(4, TxType::DctDct, 2);
    }

    #[test]
    fn fdct8x8_roundtrips_through_decoder() {
        assert_transform_roundtrips(8, TxType::DctDct, 2);
    }

    /// Microbenchmark: scalar vs AVX2 fdct8x8 (run with --ignored --nocapture).
    #[cfg(target_arch = "x86_64")]
    #[test]
    #[ignore]
    fn bench_fdct8x8() {
        if !has_avx2() {
            return;
        }
        let mut s = 0x1111_2222_3333_4444u64;
        let blocks: Vec<[i32; 64]> = (0..1000)
            .map(|_| std::array::from_fn(|_| (xs(&mut s) % 511) as i32 - 255))
            .collect();
        let mut out = [0i32; 64];
        let mut sink = 0i64;
        for (name, use_avx) in [("scalar", false), ("avx2", true)] {
            let t0 = std::time::Instant::now();
            for _ in 0..2000 {
                for b in &blocks {
                    if use_avx {
                        unsafe { fdct8x8_avx2(b, &mut out) }
                    } else {
                        fdct8x8_scalar(b, &mut out)
                    }
                    sink += out[0] as i64;
                }
            }
            let el = t0.elapsed().as_secs_f64();
            println!("{name}: {:.4} us/call", el / 2e6 * 1e6);
        }
        assert!(sink != 0);
    }

    /// Byte-identity gate for the AVX2 `fdct8x8` vs the scalar oracle, over
    /// random residuals at 8-bit (±255) and 10-bit (±1023) ranges plus the
    /// extremes (all-max, all-min, impulse) — the i32-product headroom proof.
    #[cfg(target_arch = "x86_64")]
    /// The vector matrix transform must equal the scalar reference bit for
    /// bit, for every size and ADST mix, on in-range residuals (including the
    /// 8/10/12-bit extremes and the table's own bound), and must decline
    /// anything past that bound rather than wrap.
    #[cfg(target_arch = "x86_64")]
    #[test]
    fn matrix_avx2_matches_scalar() {
        if !std::is_x86_feature_detected!("avx2") {
            return;
        }
        fn run(n: usize, residual: &[i32], rm: &MatBasis, cb: &[i64], out: &mut [i32]) -> bool {
            let mut tmp = vec![-1i64; 1024];
            // SAFETY: AVX2 checked by the caller; n is 4, 8 or 16.
            unsafe {
                match n {
                    4 => matrix_avx2::forward::<4>(residual, rm, cb, out, &mut tmp),
                    8 => matrix_avx2::forward::<8>(residual, rm, cb, out, &mut tmp),
                    _ => matrix_avx2::forward::<16>(residual, rm, cb, out, &mut tmp),
                }
            }
        }
        let mut s = 0x1234_5678_9abc_def1u64;
        for &n in &[4usize, 8, 16] {
            for &(ra, ca) in &[(false, false), (false, true), (true, false), (true, true)] {
                let (rm, cm) = (mat_basis(n, ra), mat_basis(n, ca));
                assert!(rm.lim >= 4095, "n={n} adst={ra}: lim {}", rm.lim);
                for round in 0..400 {
                    let amp = match round % 4 {
                        0 => 255i32,
                        1 => 1023,
                        2 => 4095,
                        _ => rm.lim,
                    };
                    let residual: Vec<i32> = (0..n * n)
                        .map(|i| match round {
                            0..=3 => amp,
                            4..=7 => -amp,
                            8..=11 => {
                                if i % 2 == 0 {
                                    amp
                                } else {
                                    -amp
                                }
                            }
                            _ => (xs(&mut s) % (2 * amp as u64 + 1)) as i32 - amp,
                        })
                        .collect();
                    let mut want = vec![0i32; n * n];
                    let mut got = vec![0x5a5a_5a5ai32; n * n];
                    let mut t0 = vec![0i64; 1024];
                    forward_2d_matrix_scalar(&residual, n, &rm.b, &cm.b, &mut want, &mut t0);
                    assert!(
                        run(n, &residual, rm, &cm.b, &mut got),
                        "n={n} declined an in-range residual (amp {amp})"
                    );
                    assert_eq!(got, want, "n={n} row_adst={ra} col_adst={ca} round={round}");
                }
                // One sample past the bound must be declined, not wrapped.
                let mut residual = vec![0i32; n * n];
                residual[n * n - 1] = rm.lim + 1;
                let mut got = vec![0i32; n * n];
                assert!(
                    !run(n, &residual, rm, &cm.b, &mut got),
                    "n={n}: out-of-bound residual was not declined"
                );
            }
        }
    }

    #[test]
    fn fdct8x8_avx2_matches_scalar() {
        if !has_avx2() {
            return;
        }
        let mut s = 0xfeed_beef_cafe_0001u64;
        let mut check = |residual: &[i32]| {
            let mut want = [0i32; 64];
            let mut got = [0i32; 64];
            fdct8x8_scalar(residual, &mut want);
            unsafe { fdct8x8_avx2(residual, &mut got) };
            assert_eq!(got, want);
        };
        for &range in &[255i32, 1023] {
            for _ in 0..2000 {
                let r: Vec<i32> = (0..64)
                    .map(|_| (xs(&mut s) % (2 * range as u64 + 1)) as i32 - range)
                    .collect();
                check(&r);
            }
            check(&[range; 64]);
            check(&[-range; 64]);
            let mut imp = [0i32; 64];
            imp[0] = range;
            check(&imp);
            imp[0] = 0;
            imp[63] = -range;
            check(&imp);
        }
    }

    #[test]
    fn fdct16x16_roundtrips_through_decoder() {
        assert_transform_roundtrips(16, TxType::DctDct, 3);
    }

    #[test]
    fn fdct32x32_roundtrips_through_decoder() {
        assert_transform_roundtrips(32, TxType::DctDct, 4);
    }

    #[test]
    fn fadst_and_hybrid_roundtrip_through_decoder() {
        // Every ADST/DCT mix at every ADST size must round-trip.
        for &n in &[4usize, 8, 16] {
            for &tx in &[TxType::AdstDct, TxType::DctAdst, TxType::AdstAdst] {
                assert_transform_roundtrips(n, tx, 3);
            }
        }
    }

    #[test]
    fn fwht4x4_roundtrips_exactly() {
        // The Walsh-Hadamard transform is lossless: the round-trip through the
        // decoder's `inverse_wht_add` must be bit-exact (zero error).
        use crate::transform::inverse_wht_add;
        let mut s = 0x1357_9bdfu64;
        let base = 512i32;
        for _ in 0..300 {
            let residual: Vec<i32> = (0..16).map(|_| (xs(&mut s) % 401) as i32 - 200).collect();
            let mut coeffs = [0i32; 16];
            fwht4x4(&residual, &mut coeffs);
            let mut dest = [base as u16; 16];
            inverse_wht_add(&coeffs, &mut dest, 4, 4095);
            for i in 0..16 {
                assert_eq!(dest[i] as i32 - base, residual[i], "wht pos {i}");
            }
        }
    }

    #[test]
    fn fdct4x4_dc_scale_is_exact() {
        // A flat residual `v` must produce DC == 32·v (the libvpx 4×4 scale) and
        // zero AC, and reconstruct back to exactly `v`.
        for v in [-50i32, -7, 1, 16, 63] {
            let residual = vec![v; 16];
            let mut coeffs = [0i32; 16];
            forward_transform(&residual, 4, TxType::DctDct, &mut coeffs);
            assert_eq!(coeffs[0], 32 * v, "DC for v={v}");
            assert!(
                coeffs[1..].iter().all(|&c| c == 0),
                "AC must be zero, v={v}"
            );
        }
    }
}
