//! VP9 encoder — forward quantization (Floor 1, brick Q1).
//!
//! Divide each forward-transform coefficient by the decoder's dequant step (the
//! same `DC/AC_QLOOKUP` tables, reused via [`crate::quant`]), round to nearest,
//! and emit both the integer **levels** the token coder will write and the
//! **dequantized** coefficients the reconstruction loop feeds back through the
//! inverse transform. The dequant `(level·step) >> dq_shift` is bit-identical to
//! the decoder's `decode_coefs`, so the encoder's reconstruction is exactly what
//! the decoder will produce from the same levels.

/// Quantize a forward-transformed block (`coeffs`, natural row-major order).
///
/// * `scan` — the coefficient scan, used only to place the EOB.
/// * `dc_step` / `ac_step` — dequant steps for the DC (scan pos 0) / AC coeffs.
/// * `ac_round` — the rounding offset for AC coefficients (DC always rounds to
///   nearest at `dc_step/2`). `ac_step/2` is round-to-nearest; a smaller value is
///   an RD-aware **deadzone** (rounds AC toward zero, trading a little distortion
///   for fewer bits — R5).
/// * `dq_shift` — the decoder's extra right-shift: 1 for 32×32, else 0.
///
/// Writes `levels` (signed integer levels) and `dqcoeff` (the dequantized
/// reconstruction), both natural order, and returns the EOB — the number of
/// scan positions up to and including the last non-zero level.
#[allow(clippy::too_many_arguments)]
pub fn quantize(
    coeffs: &[i32],
    scan: &[i16],
    dc_step: i32,
    ac_step: i32,
    ac_round: i64,
    dq_shift: u32,
    levels: &mut [i32],
    dqcoeff: &mut [i32],
) -> usize {
    let n = coeffs.len();
    levels[..n].fill(0);
    dqcoeff[..n].fill(0);
    let mut eob = 0usize;
    for (idx, &p) in scan.iter().enumerate() {
        let pos = p as usize;
        let (step, round) = if idx == 0 {
            (dc_step as i64, dc_step as i64 / 2)
        } else {
            (ac_step as i64, ac_round)
        };
        let coeff = coeffs[pos] as i64;
        // (|coeff|·2^dq_shift + round) / step.
        let acoef = (coeff.unsigned_abs() << dq_shift) as i64;
        let level = ((acoef + round) / step) as i32;
        if level != 0 {
            // Dequant exactly as the decoder does: shift the *magnitude*, then
            // apply the sign. A signed arithmetic shift would round toward −∞ for
            // negative coefficients (off-by-one vs the decoder when `step` is odd
            // and `dq_shift > 0`, i.e. 32×32) — see `token::decode_coefs`.
            let mag = ((level as i64 * step) >> dq_shift) as i32;
            levels[pos] = if coeff < 0 { -level } else { level };
            dqcoeff[pos] = if coeff < 0 { -mag } else { mag };
            eob = idx + 1;
        }
    }
    eob
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::quant::{ac_quant, dc_quant};
    use crate::token::get_scan;
    use crate::transform::{inverse_transform_add, TxType};

    fn xs(s: &mut u64) -> u64 {
        *s ^= *s << 13;
        *s ^= *s >> 7;
        *s ^= *s << 17;
        *s
    }

    /// T4 — the pixel↔coefficient core gate. For each size/tx/qindex:
    /// `residual → forward → quantize → dequant → inverse_transform_add → recon`.
    /// The dequant identity is exact by construction; the reconstruction error is
    /// bounded by the quantization step (the only loss), and tiny at the finest
    /// step (qindex 0).
    #[test]
    fn pixel_coeff_roundtrip_through_quant() {
        let sizes = [
            (4usize, TxType::DctDct),
            (8, TxType::DctDct),
            (16, TxType::DctDct),
            (32, TxType::DctDct),
            (4, TxType::AdstAdst),
            (8, TxType::AdstDct),
            (16, TxType::DctAdst),
        ];
        let mut s = 0xc0ff_ee00_1234_5678u64;
        let base = 512i32;
        for &(n, tx) in &sizes {
            let tx_size = (n.trailing_zeros() - 2) as usize;
            let dq_shift = if n == 32 { 1 } else { 0 };
            let (scan, _) = get_scan(tx_size, tx);
            for &qindex in &[0i32, 32, 96, 200] {
                let dc = dc_quant(qindex, 8);
                let ac = ac_quant(qindex, 8);
                let step = ac.max(dc);
                let mut max_err = 0i32;
                for _ in 0..40 {
                    let residual: Vec<i32> = (0..n * n)
                        .map(|_| (xs(&mut s) % 321) as i32 - 160)
                        .collect();
                    let mut coeffs = vec![0i32; n * n];
                    crate::encode::forward_transform(&residual, n, tx, &mut coeffs);
                    let mut levels = vec![0i32; n * n];
                    let mut dqcoeff = vec![0i32; n * n];
                    let eob = quantize(
                        &coeffs,
                        scan,
                        dc,
                        ac,
                        ac as i64 / 2, // round-to-nearest
                        dq_shift,
                        &mut levels,
                        &mut dqcoeff,
                    );

                    // Dequant identity: dqcoeff == (level·step) >> dq_shift.
                    for (idx, &p) in scan.iter().enumerate() {
                        let pos = p as usize;
                        let st = if idx == 0 { dc } else { ac };
                        let mag =
                            ((levels[pos].unsigned_abs() as i64 * st as i64) >> dq_shift) as i32;
                        let want = if levels[pos] < 0 { -mag } else { mag };
                        assert_eq!(dqcoeff[pos], want, "dequant identity {n}x{n} pos {pos}");
                        if idx >= eob {
                            assert_eq!(levels[pos], 0, "level past EOB must be zero");
                        }
                    }

                    // Reconstruct exactly as the decoder will, from dqcoeff.
                    let mut dest = vec![base as u16; n * n];
                    inverse_transform_add(&dqcoeff, n, tx, &mut dest, n, 4095);
                    for i in 0..n * n {
                        max_err = max_err.max((dest[i] as i32 - base - residual[i]).abs());
                    }
                }
                // The pixel error is bounded by the step; comfortably so.
                assert!(
                    max_err <= step,
                    "{n}x{n} {tx:?} q{qindex}: max pixel err {max_err} > step {step}"
                );
            }
        }
    }
}
