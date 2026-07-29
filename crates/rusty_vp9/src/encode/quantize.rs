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
    // Early-out threshold: `level = (|c|·2^dq_shift + round) / step ≥ 1` iff
    // `|c| ≥ ceil((step − round) / 2^dq_shift)` — any coefficient below it
    // provably quantizes to 0, so the (expensive) i64 division is skipped for
    // the sub-threshold majority. Bit-identical: the skipped path would have
    // written level 0, which the upfront fill already did.
    let ac_thresh = ((ac_step as i64 - ac_round + ((1i64 << dq_shift) - 1)) >> dq_shift).max(0);

    // AVX2 mask-scan: one vector pass flags the above-threshold positions in
    // natural order (u64 mask per 64 positions), then only the set bits are
    // visited via the inverse scan — versus walking all `n` scan positions.
    // Identical output: the threshold is exact (see above), per-position writes
    // are order-independent, and `eob = max(iscan[pos]) + 1` equals the last
    // nonzero in scan order. The scalar loop below stays as the oracle/fallback.
    #[cfg(target_arch = "x86_64")]
    if n >= 16 && std::is_x86_feature_detected!("avx2") {
        // SAFETY: AVX2 confirmed; n is a multiple of 16 (4×4 .. 32×32 blocks).
        return unsafe {
            quantize_masked_avx2(
                coeffs, scan, dc_step, ac_step, ac_round, dq_shift, ac_thresh, levels, dqcoeff,
            )
        };
    }

    quantize_scan_loop(
        coeffs, scan, dc_step, ac_step, ac_round, dq_shift, ac_thresh, levels, dqcoeff,
    )
}

/// The scan-order reference loop (early-out on the AC threshold) — the oracle
/// and non-AVX2 fallback for [`quantize`].
#[allow(clippy::too_many_arguments)]
fn quantize_scan_loop(
    coeffs: &[i32],
    scan: &[i16],
    dc_step: i32,
    ac_step: i32,
    ac_round: i64,
    dq_shift: u32,
    ac_thresh: i64,
    levels: &mut [i32],
    dqcoeff: &mut [i32],
) -> usize {
    let mut eob = 0usize;
    for (idx, &p) in scan.iter().enumerate() {
        let pos = p as usize;
        let coeff = coeffs[pos] as i64;
        let (step, round) = if idx == 0 {
            (dc_step as i64, dc_step as i64 / 2)
        } else {
            if (coeff.unsigned_abs() as i64) < ac_thresh {
                continue;
            }
            (ac_step as i64, ac_round)
        };
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

/// Inverse scan (`iscan[pos] = scan index`) for a static scan table, cached by
/// pointer identity (the scan tables are `'static`; there are ~10 of them).
fn iscan_for(scan: &[i16]) -> std::rc::Rc<[u16]> {
    use std::cell::RefCell;
    use std::rc::Rc;
    thread_local! {
        static CACHE: RefCell<Vec<(usize, Rc<[u16]>)>> = const { RefCell::new(Vec::new()) };
    }
    let key = scan.as_ptr() as usize;
    CACHE.with(|c| {
        if let Some((_, v)) = c.borrow().iter().find(|(k, _)| *k == key) {
            return v.clone();
        }
        let mut inv = vec![0u16; scan.len()];
        for (idx, &p) in scan.iter().enumerate() {
            inv[p as usize] = idx as u16;
        }
        let rc: Rc<[u16]> = inv.into();
        c.borrow_mut().push((key, rc.clone()));
        rc
    })
}

/// AVX2 mask-scan quantize: flag above-threshold positions, visit only those.
/// Bit-identical to [`quantize_scan_loop`] (gated by
/// `quantize_masked_matches_scan_loop`).
///
/// # Safety
/// AVX2 must be present; `n` a multiple of 8.
#[cfg(target_arch = "x86_64")]
#[target_feature(enable = "avx2")]
#[allow(clippy::too_many_arguments)]
unsafe fn quantize_masked_avx2(
    coeffs: &[i32],
    scan: &[i16],
    dc_step: i32,
    ac_step: i32,
    ac_round: i64,
    dq_shift: u32,
    ac_thresh: i64,
    levels: &mut [i32],
    dqcoeff: &mut [i32],
) -> usize {
    use std::arch::x86_64::*;
    let n = coeffs.len();
    let iscan = iscan_for(scan);
    let mut eob = 0usize;

    // DC (scan index 0 == position 0 in every VP9 scan): always evaluated, with
    // its own round-to-nearest offset.
    let dc = coeffs[0] as i64;
    if dc != 0 {
        let level = (((dc.unsigned_abs() << dq_shift) as i64 + dc_step as i64 / 2)
            / dc_step as i64) as i32;
        if level != 0 {
            let mag = ((level as i64 * dc_step as i64) >> dq_shift) as i32;
            levels[0] = if dc < 0 { -level } else { level };
            dqcoeff[0] = if dc < 0 { -mag } else { mag };
            eob = 1;
        }
    }

    // AC: chunked 64-bit masks of |c| ≥ ac_thresh, natural position order.
    let cmp_bound = _mm256_set1_epi32((ac_thresh as i32).saturating_sub(1));
    let mut base = 0usize;
    while base < n {
        let mut mask = 0u64;
        let lanes = (n - base).min(64);
        let mut off = 0usize;
        while off < lanes {
            let v = _mm256_loadu_si256(coeffs.as_ptr().add(base + off) as *const __m256i);
            let a = _mm256_abs_epi32(v);
            let m = _mm256_movemask_ps(_mm256_castsi256_ps(_mm256_cmpgt_epi32(a, cmp_bound)));
            mask |= (m as u32 as u64) << off;
            off += 8;
        }
        if base == 0 {
            mask &= !1u64; // DC handled above
        }
        while mask != 0 {
            let bit = mask.trailing_zeros() as usize;
            mask &= mask - 1;
            let pos = base + bit;
            let coeff = coeffs[pos] as i64;
            let acoef = (coeff.unsigned_abs() << dq_shift) as i64;
            let level = ((acoef + ac_round) / ac_step as i64) as i32;
            if level != 0 {
                let mag = ((level as i64 * ac_step as i64) >> dq_shift) as i32;
                levels[pos] = if coeff < 0 { -level } else { level };
                dqcoeff[pos] = if coeff < 0 { -mag } else { mag };
                eob = eob.max(iscan[pos] as usize + 1);
            }
        }
        base += 64;
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

    /// The AVX2 mask-scan must produce identical (levels, dqcoeff, eob) to the
    /// scan-order reference loop across sizes, qindexes (thresholds), deadzone
    /// roundings, densities, and the dq_shift=1 (32×32) path.
    #[cfg(target_arch = "x86_64")]
    #[test]
    fn quantize_masked_matches_scan_loop() {
        if !std::is_x86_feature_detected!("avx2") {
            return;
        }
        let mut s = 0x5151_a3a3_7777_0202u64;
        for &(n, tx) in &[
            (4usize, TxType::DctDct),
            (4, TxType::AdstAdst),
            (8, TxType::DctDct),
            (8, TxType::AdstDct),
            (16, TxType::DctDct),
            (32, TxType::DctDct),
        ] {
            let tx_size = (n.trailing_zeros() - 2) as usize;
            let dq_shift = if n == 32 { 1u32 } else { 0 };
            let (scan, _) = get_scan(tx_size, tx);
            for &qindex in &[0i32, 32, 96, 200, 255] {
                let dc = dc_quant(qindex, 8);
                let ac = ac_quant(qindex, 8);
                for &round_div in &[2i64, 3] {
                    let ac_round = ac as i64 / round_div; // nearest + deadzone
                    let ac_thresh =
                        ((ac as i64 - ac_round + ((1i64 << dq_shift) - 1)) >> dq_shift).max(0);
                    for density in [1u64, 4, 16] {
                        let coeffs: Vec<i32> = (0..n * n)
                            .map(|_| {
                                if xs(&mut s) % density != 0 {
                                    0
                                } else {
                                    (xs(&mut s) % 4001) as i32 - 2000
                                }
                            })
                            .collect();
                        let mut l1 = vec![0i32; n * n];
                        let mut d1 = vec![0i32; n * n];
                        let mut l2 = vec![0i32; n * n];
                        let mut d2 = vec![0i32; n * n];
                        let e1 = quantize_scan_loop(
                            &coeffs, scan, dc, ac, ac_round, dq_shift, ac_thresh, &mut l1,
                            &mut d1,
                        );
                        let e2 = unsafe {
                            quantize_masked_avx2(
                                &coeffs, scan, dc, ac, ac_round, dq_shift, ac_thresh, &mut l2,
                                &mut d2,
                            )
                        };
                        assert_eq!(e1, e2, "eob {n}x{n} q{qindex} rd{round_div}");
                        assert_eq!(l1, l2, "levels {n}x{n} q{qindex}");
                        assert_eq!(d1, d2, "dqcoeff {n}x{n} q{qindex}");
                    }
                }
            }
        }
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
