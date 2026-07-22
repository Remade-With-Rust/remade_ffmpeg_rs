//! Laplacian rate-distortion model — libvpx `vp9_model_rd_from_var_lapndz`.
//!
//! Estimates the RATE (bits) and DISTORTION (SSE) of coding a block's residual
//! from its variance and the quantizer step, WITHOUT running the forward transform
//! + quantize + token trial. This is the model behind libvpx's non-RD mode decision
//! (`nonrd_pick_mode`): rank candidates and decide skip from a cheap closed-form
//! estimate, transforming only the committed winner.
//!
//! It is the principled alternative to a raw residual-SSE threshold: the Laplacian
//! source model captures how a uniform quantizer of step `qstep` turns residual
//! energy into bits (near-zero when `qstep ≫ √var`, i.e. the block skips), which a
//! mean-SSE gate cannot. Ported verbatim (tables + fixed-point path) so it matches
//! libvpx's behaviour; only the output units are re-expressed as (bits, SSE) to feed
//! our `J = SSE + λ·bits` RD directly.

// Normalized rate: models `n · H(exp(-√2·x))` for a Laplacian source, x = qstep/√var.
// (libvpx `rate_tab_q10` — per-pixel rate in Q10 bits, indexed by the quantized xsq.)
const RATE_TAB_Q10: &[i32] = & [
    65536, 6086, 5574, 5275, 5063, 4899, 4764, 4651, 4553, 4389, 4255, 4142, 4044, 3958, 3881,
    3811, 3748, 3635, 3538, 3453, 3376, 3307, 3244, 3186, 3133, 3037, 2952, 2877, 2809, 2747,
    2690, 2638, 2589, 2501, 2423, 2353, 2290, 2232, 2179, 2130, 2084, 2001, 1928, 1862, 1802,
    1748, 1698, 1651, 1608, 1530, 1460, 1398, 1342, 1290, 1243, 1199, 1159, 1086, 1021, 963,
    911, 864, 821, 781, 745, 680, 623, 574, 530, 490, 455, 424, 395, 345, 304, 269, 239, 213,
    190, 171, 154, 126, 104, 87, 73, 61, 52, 44, 38, 28, 21, 16, 12, 10, 8, 6, 5, 3, 2, 1, 1, 1,
    0, 0,
];

// Normalized distortion Dn(x) in Q10 (actual distortion = Dn · var). libvpx `dist_tab_q10`.
const DIST_TAB_Q10: &[i32] = & [
    0, 0, 1, 1, 1, 2, 2, 2, 3, 3, 4, 5, 5, 6, 7, 7, 8, 9, 11, 12, 13, 15, 16, 17, 18, 21, 24, 26,
    29, 31, 34, 36, 39, 44, 49, 54, 59, 64, 69, 73, 78, 88, 97, 106, 115, 124, 133, 142, 151, 167,
    184, 200, 215, 231, 245, 260, 274, 301, 327, 351, 375, 397, 418, 439, 458, 495, 528, 559, 587,
    613, 637, 659, 680, 717, 749, 777, 801, 823, 842, 859, 874, 899, 919, 936, 949, 960, 969, 977,
    983, 994, 1001, 1006, 1010, 1013, 1015, 1017, 1018, 1020, 1022, 1022, 1023, 1023, 1023, 1024,
];

// Inverse-quantized xsq breakpoints (libvpx `xsq_iq_q10`) for the piecewise interp.
const XSQ_IQ_Q10: &[i32] = & [
    0, 4, 8, 12, 16, 20, 24, 28, 32, 40, 48, 56, 64, 72, 80, 88, 96, 112, 128, 144, 160, 176, 192,
    208, 224, 256, 288, 320, 352, 384, 416, 448, 480, 544, 608, 672, 736, 800, 864, 928, 992,
    1120, 1248, 1376, 1504, 1632, 1760, 1888, 2016, 2272, 2528, 2784, 3040, 3296, 3552, 3808, 4064,
    4576, 5088, 5600, 6112, 6624, 7136, 7648, 8160, 9184, 10208, 11232, 12256, 13280, 14304, 15328,
    16352, 18400, 20448, 22496, 24544, 26592, 28640, 30688, 32736, 36832, 40928, 45024, 49120,
    53216, 57312, 61408, 65504, 73696, 81888, 90080, 98272, 106464, 114656, 122848, 131040, 147424,
    163808, 180192, 196576, 212960, 229344, 245728,
];

const MAX_XSQ_Q10: u64 = 245727;

#[inline]
fn get_msb(x: u32) -> u32 {
    31 - x.leading_zeros()
}

/// libvpx `model_rd_norm`: normalized (rate, dist) in Q10 for quantized `xsq_q10`.
#[inline]
fn model_rd_norm(xsq_q10: i32) -> (i32, i32) {
    let tmp = (xsq_q10 >> 2) + 8;
    let k = (get_msb(tmp as u32) as i32) - 3;
    let xq = ((k << 3) + ((tmp >> k) & 0x7)) as usize;
    let one_q10 = 1 << 10;
    let a_q10 = ((xsq_q10 - XSQ_IQ_Q10[xq]) << 10) >> (2 + k);
    let b_q10 = one_q10 - a_q10;
    let r_q10 = (RATE_TAB_Q10[xq] * b_q10 + RATE_TAB_Q10[xq + 1] * a_q10) >> 10;
    let d_q10 = (DIST_TAB_Q10[xq] * b_q10 + DIST_TAB_Q10[xq + 1] * a_q10) >> 10;
    (r_q10, d_q10)
}

/// The model's normalized `xsq` feature = `qstep²·pixels·2^10 / sse` (Q10), the
/// squared quantizer-to-per-sample-std ratio. Bigger ⇒ the residual is small relative
/// to the quantizer ⇒ the block is more likely to code to (near) nothing (skip). This
/// is the RIGHT skip FEATURE; the skip THRESHOLD on it is calibrated per encoder (the
/// model's own rate/dist tables mis-predict our quantizer, so we gate on xsq directly).
/// `sse == 0` ⇒ `u64::MAX` (a certain skip).
pub fn model_xsq(sse: u64, n_log2: u32, qstep: i64) -> u64 {
    if sse == 0 {
        return u64::MAX;
    }
    let q2 = (qstep as u64) * (qstep as u64);
    ((q2 << (n_log2 + 10)) + (sse >> 1)) / sse
}

/// Model the (rate in BITS, distortion in SSE) of coding a block whose residual has
/// total squared error `sse` over `2^n_log2` pixels, at dequant step `qstep`.
/// Returns (0, 0) when `sse == 0` (an exactly-predicted block — a certain skip).
pub fn model_rd(sse: u64, n_log2: u32, qstep: i64) -> (f64, f64) {
    if sse == 0 {
        return (0.0, 0.0);
    }
    let q2 = (qstep as u64) * (qstep as u64);
    let xsq_q10_64 = (q2 << (n_log2 + 10)) + (sse >> 1);
    let xsq_q10 = (xsq_q10_64 / sse).min(MAX_XSQ_Q10) as i32;
    let (r_q10, d_q10) = model_rd_norm(xsq_q10);
    // r_q10 is per-pixel rate in Q10 bits → total bits = r_q10 · pixels / 1024.
    let rate_bits = (r_q10 as f64) * (1u64 << n_log2) as f64 / 1024.0;
    // Dn is Q10 → distortion = sse · Dn / 1024.
    let dist_sse = (sse as f64) * (d_q10 as f64) / 1024.0;
    (rate_bits, dist_sse)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn zero_residual_is_free() {
        assert_eq!(model_rd(0, 6, 40), (0.0, 0.0));
    }

    #[test]
    fn monotone_in_residual() {
        // More residual energy ⇒ more bits and more distortion (at a fixed qstep/size).
        let (r_lo, d_lo) = model_rd(1_000, 6, 40);
        let (r_hi, d_hi) = model_rd(100_000, 6, 40);
        assert!(r_hi > r_lo, "rate not monotone: {r_lo} !< {r_hi}");
        assert!(d_hi > d_lo, "dist not monotone: {d_lo} !< {d_hi}");
    }

    #[test]
    fn big_qstep_skips() {
        // qstep ≫ √var ⇒ modeled rate collapses toward 0 (the block skips) and the
        // distortion approaches the full residual energy (no coefficients coded).
        let sse = 4_000u64; // 64px × ~8²  → per-pixel var ~62, √var ~8
        let (rate, dist) = model_rd(sse, 6, 200); // qstep 200 ≫ 8
        assert!(rate < 1.0, "expected near-skip rate, got {rate}");
        assert!(dist > sse as f64 * 0.9, "expected ~full distortion, got {dist}");
    }

    #[test]
    fn small_qstep_codes_bits() {
        // qstep ≪ √var ⇒ many bits, little distortion.
        let sse = 400_000u64; // per-pixel var ~6250, √var ~79
        let (rate, dist) = model_rd(sse, 6, 8); // qstep 8 ≪ 79
        assert!(rate > 10.0, "expected many bits, got {rate}");
        assert!(dist < sse as f64 * 0.2, "expected low distortion, got {dist}");
    }
}
