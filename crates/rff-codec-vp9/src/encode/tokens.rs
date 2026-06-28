//! VP9 encoder — coefficient token coding (Floor 2, brick B1).
//!
//! [`encode_coefs`] is the exact inverse of the decoder's
//! [`decode_coefs`](crate::token::decode_coefs): it walks the same scan order,
//! emitting per position an EOB / zero-run / token-tree decision, the category
//! extra bits, and the sign — through the [`BoolEncoder`]. It reuses the
//! decoder's context derivation (`get_coef_context`), scan/neighbour/band
//! tables, Pareto expansion and category probabilities verbatim, and accumulates
//! the identical symbol counts. A round-trip through `decode_coefs` recovers the
//! exact dequantized block, EOB, and counts.

use std::sync::OnceLock;

use super::bitwriter::BoolEncoder;
use crate::prob_tables::{
    CAT1_PROB, CAT2_PROB, CAT3_PROB, CAT4_PROB, CAT5_PROB, CAT6_PROB, CAT6_PROB_HIGH10,
    CAT6_PROB_HIGH12, PARETO8_FULL,
};
use crate::scan_tables::{COEFBAND_4X4, COEFBAND_8X8PLUS};
use crate::token::get_coef_context;

/// A destination for boolean symbols: either emit them (encode) or accumulate
/// their bit cost (RDO). Both B1 and B2 drive the *same* tree through this, so
/// the cost can never drift from what the encoder actually writes.
trait BitSink {
    fn put(&mut self, bit: u32, prob: u8);
}

impl BitSink for BoolEncoder {
    #[inline]
    fn put(&mut self, bit: u32, prob: u8) {
        self.write_bool(bit, prob);
    }
}

/// Boolean-coder bit cost in Q8 (256ths of a bit): `cost_q8[q] = -log2(q/256)·256`.
/// `P(bit=0) ≈ prob/256`, `P(bit=1) ≈ (256-prob)/256`.
fn cost_table() -> &'static [u16; 257] {
    static T: OnceLock<[u16; 257]> = OnceLock::new();
    T.get_or_init(|| {
        let mut t = [0u16; 257];
        for (q, slot) in t.iter_mut().enumerate().skip(1) {
            *slot = (-(q as f64 / 256.0).log2() * 256.0).round() as u16;
        }
        t[0] = t[1];
        t
    })
}

#[inline]
fn cost_bit(prob: u8, bit: u32) -> u64 {
    let t = cost_table();
    if bit == 0 {
        t[prob as usize] as u64
    } else {
        t[256 - prob as usize] as u64
    }
}

/// Cost-accumulating sink (Q8 bits) for B2.
struct CostSink(u64);

impl BitSink for CostSink {
    #[inline]
    fn put(&mut self, bit: u32, prob: u8) {
        self.0 += cost_bit(prob, bit);
    }
}

/// Code an `n`-bit magnitude MSB-first — the inverse of the decoder's `read_coeff`.
#[inline]
fn code_extra<S: BitSink>(sink: &mut S, value: u32, probs: &[u8], n: usize) {
    for i in 0..n {
        sink.put((value >> (n - 1 - i)) & 1, probs[i]);
    }
}

/// Code the magnitude `aval` (≥ 1) of a non-zero coefficient through the token
/// tree (inverse of `decode_coefs`'s token branch). `prob2` is the model pivot
/// node, which also indexes the Pareto tail. Returns the energy class to store
/// in the token cache (matching the decoder).
fn code_magnitude<S: BitSink>(sink: &mut S, aval: u32, prob2: u8, cat6: &[u8], cat6_bits: usize) -> u8 {
    if aval == 1 {
        sink.put(0, prob2); // ONE
        return 1;
    }
    sink.put(1, prob2); // TWO+
    let p = &PARETO8_FULL[prob2 as usize - 1];
    if aval <= 4 {
        sink.put(0, p[0]);
        if aval == 2 {
            sink.put(0, p[1]);
            2
        } else {
            sink.put(1, p[1]);
            sink.put(aval - 3, p[2]); // 3→0, 4→1
            3
        }
    } else {
        sink.put(1, p[0]);
        if aval <= 10 {
            sink.put(0, p[3]);
            if aval <= 6 {
                sink.put(0, p[4]);
                code_extra(sink, aval - 5, &CAT1_PROB, 1);
            } else {
                sink.put(1, p[4]);
                code_extra(sink, aval - 7, &CAT2_PROB, 2);
            }
            4
        } else {
            sink.put(1, p[3]);
            if aval <= 34 {
                sink.put(0, p[5]);
                if aval <= 18 {
                    sink.put(0, p[6]);
                    code_extra(sink, aval - 11, &CAT3_PROB, 3);
                } else {
                    sink.put(1, p[6]);
                    code_extra(sink, aval - 19, &CAT4_PROB, 4);
                }
            } else {
                sink.put(1, p[5]);
                if aval <= 66 {
                    sink.put(0, p[7]);
                    code_extra(sink, aval - 35, &CAT5_PROB, 5);
                } else {
                    sink.put(1, p[7]);
                    code_extra(sink, aval - 67, cat6, cat6_bits);
                }
            }
            5
        }
    }
}

/// The shared coefficient-block walk used by both [`encode_coefs`] (emit) and
/// [`coef_cost`] (cost). Mirrors `decode_coefs` exactly, including the token
/// cache, context derivation and symbol counts.
#[allow(clippy::too_many_arguments)]
fn code_block<S: BitSink>(
    sink: &mut S,
    levels: &[i32],
    scan: &[i16],
    nb: &[i16],
    eob: usize,
    coef_probs: &[[[u8; 3]; 6]; 6],
    tx_size: usize,
    mut ctx: usize,
    token_cache: &mut [u8],
    coef_cnt: &mut [[[u32; 4]; 6]; 6],
    eob_cnt: &mut [[u32; 6]; 6],
    bit_depth: u32,
) {
    let max_eob = 16usize << (tx_size << 1);
    let band_translate: &[u8] = if tx_size == 0 {
        &COEFBAND_4X4
    } else {
        &COEFBAND_8X8PLUS
    };
    let (cat6, cat6_bits): (&[u8], usize) = match bit_depth {
        10 => (&CAT6_PROB_HIGH10, 16),
        12 => (&CAT6_PROB_HIGH12, 18),
        _ => (&CAT6_PROB, 14),
    };
    token_cache[..max_eob].fill(0);

    let mut c = 0usize;
    while c < max_eob {
        let band = band_translate[c] as usize;
        eob_cnt[band][ctx] += 1;
        if c == eob {
            // End of block: no more non-zero coefficients.
            sink.put(0, coef_probs[band][ctx][0]);
            coef_cnt[band][ctx][3] += 1; // EOB_MODEL_TOKEN
            break;
        }
        sink.put(1, coef_probs[band][ctx][0]); // not EOB

        // Zero-run, then the non-zero coefficient (the inner `while` of decode).
        loop {
            let band = band_translate[c] as usize;
            let pos = scan[c] as usize;
            if levels[pos] == 0 {
                sink.put(0, coef_probs[band][ctx][1]); // ZERO
                coef_cnt[band][ctx][0] += 1;
                token_cache[pos] = 0;
                c += 1;
                ctx = get_coef_context(nb, token_cache, c);
            } else {
                sink.put(1, coef_probs[band][ctx][1]); // non-zero
                break;
            }
        }

        let band = band_translate[c] as usize;
        let pos = scan[c] as usize;
        let lvl = levels[pos];
        let aval = lvl.unsigned_abs();
        coef_cnt[band][ctx][if aval >= 2 { 2 } else { 1 }] += 1; // TWO+ / ONE
        let class = code_magnitude(sink, aval, coef_probs[band][ctx][2], cat6, cat6_bits);
        token_cache[pos] = class;
        sink.put((lvl < 0) as u32, 128); // sign
        c += 1;
        ctx = get_coef_context(nb, token_cache, c);
    }
}

/// Encode one transform block's coefficient `levels` (signed, natural row-major
/// order) — the inverse of [`decode_coefs`](crate::token::decode_coefs).
///
/// * `scan` / `nb` — the coefficient scan + neighbour table for this block.
/// * `eob` — scan positions through the last non-zero level (from `quantize`).
/// * `coef_probs` — `[band][ctx][3]` model probs for this (tx, plane, ref).
/// * `ctx` — initial above/left context (0..=2).
/// * `token_cache` — scratch (`>= max_eob`), maintained as the decoder does.
/// * `coef_cnt` / `eob_cnt` — symbol counts, accumulated identically to decode.
#[allow(clippy::too_many_arguments)]
pub fn encode_coefs(
    enc: &mut BoolEncoder,
    levels: &[i32],
    scan: &[i16],
    nb: &[i16],
    eob: usize,
    coef_probs: &[[[u8; 3]; 6]; 6],
    tx_size: usize,
    ctx: usize,
    token_cache: &mut [u8],
    coef_cnt: &mut [[[u32; 4]; 6]; 6],
    eob_cnt: &mut [[u32; 6]; 6],
    bit_depth: u32,
) {
    code_block(
        enc, levels, scan, nb, eob, coef_probs, tx_size, ctx, token_cache, coef_cnt, eob_cnt,
        bit_depth,
    );
}

/// Estimate the boolean-coder bit cost (Q8 — 256ths of a bit) of a coefficient
/// block **without emitting** — the RDO inner-loop oracle (brick B2). Walks the
/// identical tree as [`encode_coefs`], so its total equals what the encoder
/// actually spends.
#[allow(clippy::too_many_arguments)]
pub fn coef_cost(
    levels: &[i32],
    scan: &[i16],
    nb: &[i16],
    eob: usize,
    coef_probs: &[[[u8; 3]; 6]; 6],
    tx_size: usize,
    ctx: usize,
    token_cache: &mut [u8],
    bit_depth: u32,
) -> u64 {
    let mut sink = CostSink(0);
    let mut cc = [[[0u32; 4]; 6]; 6];
    let mut ec = [[0u32; 6]; 6];
    code_block(
        &mut sink, levels, scan, nb, eob, coef_probs, tx_size, ctx, token_cache, &mut cc, &mut ec,
        bit_depth,
    );
    sink.0
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::bits::BoolDecoder;
    use crate::quant::{ac_quant, dc_quant};
    use crate::token::{decode_coefs, default_coef_probs, get_scan};
    use crate::transform::TxType;

    fn xs(s: &mut u64) -> u64 {
        *s ^= *s << 13;
        *s ^= *s >> 7;
        *s ^= *s << 17;
        *s
    }

    /// A magnitude weighted toward small values but reaching every CAT range
    /// (so the full token tree, including CAT6, is exercised).
    fn rand_mag(s: &mut u64) -> u32 {
        match xs(s) % 100 {
            0..=70 => 1 + (xs(s) % 4) as u32,    // 1..4 (ONE/TWO/THREE/FOUR)
            71..=85 => 5 + (xs(s) % 6) as u32,   // 5..10 (CAT1/CAT2)
            86..=94 => 11 + (xs(s) % 24) as u32, // 11..34 (CAT3/CAT4)
            95..=98 => 35 + (xs(s) % 32) as u32, // 35..66 (CAT5)
            _ => 67 + (xs(s) % 4000) as u32,     // CAT6
        }
    }

    /// A random coefficient block: a valid EOB with the last scan position
    /// forced non-zero, ~40% interior zeros, magnitudes across every CAT range.
    fn gen_block(s: &mut u64, scan: &[i16], n: usize, max_eob: usize) -> (Vec<i32>, usize) {
        let eob = 1 + (xs(s) as usize % max_eob);
        let mut levels = vec![0i32; n * n];
        for c in 0..eob {
            let pos = scan[c] as usize;
            if c != eob - 1 && xs(s) % 5 < 2 {
                continue;
            }
            let mag = rand_mag(s) as i32;
            levels[pos] = if xs(s) & 1 == 0 { mag } else { -mag };
        }
        if levels[scan[eob - 1] as usize] == 0 {
            levels[scan[eob - 1] as usize] = 1;
        }
        (levels, eob)
    }

    #[test]
    fn encode_coefs_roundtrips_through_decoder() {
        let cases = [
            (0usize, TxType::DctDct),
            (0, TxType::AdstDct),
            (0, TxType::DctAdst),
            (1, TxType::DctDct),
            (2, TxType::DctDct),
            (3, TxType::DctDct),
        ];
        let mut s = 0x5eed_1234_abcd_0001u64;
        for &(tx_size, tx) in &cases {
            let n = 4usize << tx_size;
            let max_eob = 16usize << (tx_size << 1);
            let (scan, nb) = get_scan(tx_size, tx);
            let coef_probs = default_coef_probs(tx_size, 0, 0);
            let (dc, ac) = (dc_quant(40, 8), ac_quant(40, 8));
            let dq_shift = if tx_size == 3 { 1 } else { 0 };
            for _ in 0..150 {
                let (levels, eob) = gen_block(&mut s, scan, n, max_eob);
                let ctx0 = (xs(&mut s) % 3) as usize;
                // Encode.
                let mut enc = BoolEncoder::new();
                let mut tc_e = vec![0u8; max_eob];
                let mut cc_e = [[[0u32; 4]; 6]; 6];
                let mut ec_e = [[0u32; 6]; 6];
                encode_coefs(
                    &mut enc, &levels, scan, nb, eob, coef_probs, tx_size, ctx0, &mut tc_e,
                    &mut cc_e, &mut ec_e, 8,
                );
                let bytes = enc.finish();

                // Decode.
                let mut bd = BoolDecoder::new(&bytes).unwrap();
                let mut dqcoeff = vec![0i32; max_eob];
                let mut tc_d = vec![0u8; max_eob];
                let mut cc_d = [[[0u32; 4]; 6]; 6];
                let mut ec_d = [[0u32; 6]; 6];
                let (c, _) = decode_coefs(
                    &mut bd, coef_probs, tx_size, scan, nb, (dc, ac), ctx0, &mut dqcoeff,
                    &mut tc_d, &mut cc_d, &mut ec_d, 8,
                );

                assert_eq!(c, eob, "eob {tx_size} {tx:?}");
                for pos in 0..n * n {
                    let lvl = levels[pos];
                    let step = if pos == 0 { dc } else { ac } as i64;
                    let want = if lvl == 0 {
                        0
                    } else {
                        let v = ((lvl.unsigned_abs() as i64 * step) >> dq_shift) as i32;
                        if lvl < 0 {
                            -v
                        } else {
                            v
                        }
                    };
                    assert_eq!(dqcoeff[pos], want, "dqcoeff {tx_size} {tx:?} pos {pos}");
                }
                assert_eq!(cc_e, cc_d, "coef counts {tx_size} {tx:?}");
                assert_eq!(ec_e, ec_d, "eob counts {tx_size} {tx:?}");
            }
        }
    }

    #[test]
    fn coef_cost_predicts_emitted_bits() {
        // B2: the summed cost (without emitting) must predict the bits the bool
        // coder actually spends. Encode many blocks into one stream and compare
        // the predicted total to the real output size.
        let cases = [
            (0usize, TxType::DctDct),
            (1, TxType::DctDct),
            (2, TxType::DctDct),
            (3, TxType::DctDct),
        ];
        let mut s = 0x2024_0a0b_0c0d_0e0fu64;
        let mut enc = BoolEncoder::new();
        let mut total_cost_q8 = 0u64;
        for &(tx_size, tx) in &cases {
            let n = 4usize << tx_size;
            let max_eob = 16usize << (tx_size << 1);
            let (scan, nb) = get_scan(tx_size, tx);
            let coef_probs = default_coef_probs(tx_size, 0, 0);
            for _ in 0..400 {
                let (levels, eob) = gen_block(&mut s, scan, n, max_eob);
                let ctx0 = (xs(&mut s) % 3) as usize;
                let mut tc = vec![0u8; max_eob];
                total_cost_q8 +=
                    coef_cost(&levels, scan, nb, eob, coef_probs, tx_size, ctx0, &mut tc, 8);
                let mut tc2 = vec![0u8; max_eob];
                let mut cc = [[[0u32; 4]; 6]; 6];
                let mut ec = [[0u32; 6]; 6];
                encode_coefs(
                    &mut enc, &levels, scan, nb, eob, coef_probs, tx_size, ctx0, &mut tc2, &mut cc,
                    &mut ec, 8,
                );
            }
        }
        let actual_bits = enc.finish().len() as f64 * 8.0;
        let predicted_bits = total_cost_q8 as f64 / 256.0;
        let rel = (predicted_bits - actual_bits).abs() / actual_bits;
        // The bool coder achieves close to the entropy; a thin margin covers the
        // coding loss + the one-time marker/flush overhead.
        assert!(
            rel < 0.01,
            "cost prediction off by {:.3}% (predicted {predicted_bits:.0} vs actual {actual_bits:.0})",
            rel * 100.0
        );
    }
}
