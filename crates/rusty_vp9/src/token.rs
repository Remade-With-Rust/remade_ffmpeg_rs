//! VP9 coefficient (token) decoding — ISO/VP9 §8.6.4 / libvpx
//! `vp9_detokenize.c::decode_coefs`. Walks the scan order of a transform block,
//! reading per-position an EOB / ZERO / token decision from the boolean
//! decoder, then category extra bits and the sign, dequantizing into the
//! coefficient block. The probability context for each position comes from the
//! energy class of already-decoded neighbours (`get_coef_context`).
//!
//! All fixed data (scan orders, neighbour tables, band maps) lives in
//! `scan_tables.rs`, extracted from libvpx and validated; the model coefficient
//! probabilities and Pareto expansion live in `prob_tables.rs`.

#![allow(dead_code)]

use crate::bits::BoolDecoder;
use crate::prob_tables::*;
use crate::scan_tables::*;
use crate::transform::TxType;

// Category base values (CATn_MIN_VAL) and their extra-bit probability tables.
const CAT_MIN_VAL: [i32; 6] = [5, 7, 11, 19, 35, 67];

/// Select (scan, neighbours) for a transform block. Key-frame luma uses the
/// tx_type-specific scan; chroma (and lossless / inter) use the default scan.
/// Mapping (libvpx `vp9_scan_orders`): DCT_DCT & ADST_ADST → default,
/// ADST_DCT → row, DCT_ADST → col. 32×32 is always the default scan.
pub fn get_scan(tx_size: usize, tx_type: TxType) -> (&'static [i16], &'static [i16]) {
    match tx_size {
        0 => match tx_type {
            TxType::DctDct | TxType::AdstAdst => (&DEFAULT_SCAN_4X4, &DEFAULT_SCAN_4X4_NEIGHBORS),
            TxType::AdstDct => (&ROW_SCAN_4X4, &ROW_SCAN_4X4_NEIGHBORS),
            TxType::DctAdst => (&COL_SCAN_4X4, &COL_SCAN_4X4_NEIGHBORS),
        },
        1 => match tx_type {
            TxType::DctDct | TxType::AdstAdst => (&DEFAULT_SCAN_8X8, &DEFAULT_SCAN_8X8_NEIGHBORS),
            TxType::AdstDct => (&ROW_SCAN_8X8, &ROW_SCAN_8X8_NEIGHBORS),
            TxType::DctAdst => (&COL_SCAN_8X8, &COL_SCAN_8X8_NEIGHBORS),
        },
        2 => match tx_type {
            TxType::DctDct | TxType::AdstAdst => {
                (&DEFAULT_SCAN_16X16, &DEFAULT_SCAN_16X16_NEIGHBORS)
            }
            TxType::AdstDct => (&ROW_SCAN_16X16, &ROW_SCAN_16X16_NEIGHBORS),
            TxType::DctAdst => (&COL_SCAN_16X16, &COL_SCAN_16X16_NEIGHBORS),
        },
        _ => (&DEFAULT_SCAN_32X32, &DEFAULT_SCAN_32X32_NEIGHBORS),
    }
}

/// Coefficient context from the two causal neighbours' energy classes
/// (libvpx `get_coef_context`): `(1 + cache[nb[2c]] + cache[nb[2c+1]]) >> 1`.
#[inline]
pub(crate) fn get_coef_context(nb: &[i16], cache: &[u8], c: usize) -> usize {
    ((1 + cache[nb[2 * c] as usize] as usize + cache[nb[2 * c + 1] as usize] as usize) >> 1)
        as usize
}

/// Read an `n`-bit magnitude MSB-first, each bit with its own probability
/// (libvpx `read_coeff`).
#[inline]
fn read_coeff(bd: &mut BoolDecoder, probs: &[u8], n: usize) -> i32 {
    let mut val = 0i32;
    for i in 0..n {
        val = (val << 1) | bd.read_bool(probs[i]) as i32;
    }
    val
}

/// Decode one transform block's coefficients into `dqcoeff` (row-major, the
/// scan maps index→position). Returns the end-of-block count (number of
/// coefficients before EOB). Exact port of libvpx `decode_coefs`.
///
/// * `coef_probs` — `[band][ctx][node]` model probs for this (tx, plane, ref).
/// * `dq` — `(dc_quant, ac_quant)`.
/// * `ctx` — initial above/left context (0..=2).
#[allow(clippy::too_many_arguments)]
#[allow(clippy::too_many_arguments)]
pub fn decode_coefs(
    bd: &mut BoolDecoder,
    coef_probs: &[[[u8; 3]; 6]; 6],
    tx_size: usize,
    scan: &[i16],
    nb: &[i16],
    dq: (i32, i32),
    mut ctx: usize,
    dqcoeff: &mut [i32],
    token_cache: &mut [u8],
    coef_cnt: &mut [[[u32; 4]; 6]; 6],
    eob_cnt: &mut [[u32; 6]; 6],
    bit_depth: u32,
) -> (usize, usize) {
    let max_eob = 16usize << (tx_size << 1);
    let n = 4usize << tx_size; // transform side; row = pos / n
    let mut max_row = 0usize; // highest row index holding a non-zero coefficient
    let band_translate: &[u8] = if tx_size == 0 {
        &COEFBAND_4X4
    } else {
        &COEFBAND_8X8PLUS
    };
    let dq_shift = if tx_size == 3 { 1 } else { 0 };
    // Category-6 token reads `14 + (bd-8)` extra bits with a bit-depth-specific
    // probability table (libvpx `cat6_prob` / `cat6_prob_high10/12`).
    let (cat6_prob, cat6_bits): (&[u8], usize) = match bit_depth {
        10 => (&CAT6_PROB_HIGH10, 16),
        12 => (&CAT6_PROB_HIGH12, 18),
        _ => (&CAT6_PROB, 14),
    };
    let mut dqv = dq.0; // DC quant for the first coefficient
                        // Caller passes reusable scratch; clear only the live `max_eob` prefix (the
                        // tail is never read — neighbour positions are always `< max_eob`).
    dqcoeff[..max_eob].iter_mut().for_each(|v| *v = 0);
    token_cache[..max_eob].iter_mut().for_each(|v| *v = 0);

    let mut c = 0usize;
    while c < max_eob {
        let mut band = band_translate[c] as usize;
        let mut prob = coef_probs[band][ctx];
        // EOB branch is visited every position; EOB_MODEL token counted on exit.
        eob_cnt[band][ctx] += 1;
        if bd.read_bool(prob[0]) == 0 {
            coef_cnt[band][ctx][3] += 1; // EOB_MODEL_TOKEN
            break;
        }
        // Run of ZERO tokens.
        while bd.read_bool(prob[1]) == 0 {
            coef_cnt[band][ctx][0] += 1; // ZERO_TOKEN
            token_cache[scan[c] as usize] = 0;
            dqv = dq.1;
            c += 1;
            if c >= max_eob {
                return (c, max_row);
            }
            ctx = get_coef_context(nb, &token_cache, c);
            band = band_translate[c] as usize;
            prob = coef_probs[band][ctx];
        }
        let pos = scan[c] as usize;
        // Non-zero token. prob[2] is both the >=TWO decision and Pareto pivot.
        let two_plus = bd.read_bool(prob[2]) != 0;
        coef_cnt[band][ctx][if two_plus { 2 } else { 1 }] += 1; // TWO+ / ONE token
        let v: i32 = if two_plus {
            let p = &PARETO8_FULL[prob[2] as usize - 1];
            if bd.read_bool(p[0]) != 0 {
                if bd.read_bool(p[3]) != 0 {
                    token_cache[pos] = 5;
                    let val = if bd.read_bool(p[5]) != 0 {
                        if bd.read_bool(p[7]) != 0 {
                            CAT_MIN_VAL[5] + read_coeff(bd, cat6_prob, cat6_bits)
                        } else {
                            CAT_MIN_VAL[4] + read_coeff(bd, &CAT5_PROB, 5)
                        }
                    } else if bd.read_bool(p[6]) != 0 {
                        CAT_MIN_VAL[3] + read_coeff(bd, &CAT4_PROB, 4)
                    } else {
                        CAT_MIN_VAL[2] + read_coeff(bd, &CAT3_PROB, 3)
                    };
                    (val * dqv) >> dq_shift
                } else {
                    token_cache[pos] = 4;
                    let val = if bd.read_bool(p[4]) != 0 {
                        CAT_MIN_VAL[1] + read_coeff(bd, &CAT2_PROB, 2)
                    } else {
                        CAT_MIN_VAL[0] + read_coeff(bd, &CAT1_PROB, 1)
                    };
                    (val * dqv) >> dq_shift
                }
            } else if bd.read_bool(p[1]) != 0 {
                token_cache[pos] = 3;
                ((3 + bd.read_bool(p[2]) as i32) * dqv) >> dq_shift
            } else {
                token_cache[pos] = 2;
                (2 * dqv) >> dq_shift
            }
        } else {
            token_cache[pos] = 1;
            dqv >> dq_shift
        };
        // Sign bit (probability 128 = equiprobable).
        dqcoeff[pos] = if bd.read_bool(128) != 0 { -v } else { v };
        max_row = max_row.max(pos / n);
        c += 1;
        ctx = get_coef_context(nb, &token_cache, c);
        dqv = dq.1;
    }
    (c, max_row)
}

/// Default (un-updated) model coef probs for a (tx, plane, ref) — convenience
/// for tests and the initial frame context before compressed-header updates.
pub fn default_coef_probs(tx: usize, plane: usize, refr: usize) -> &'static [[[u8; 3]; 6]; 6] {
    &DEFAULT_COEF_PROBS[tx][plane][refr]
}

/// Generic boolean tree reader (libvpx `vpx_read_tree`): walk a tree encoded as
/// pairs of indices, branching on `read_bool(prob[i >> 1])`; a non-negative
/// entry is a leaf (negated token), a positive entry is the next node index.
/// Used by the partition / intra-mode decoders (stage E).
pub fn read_tree(bd: &mut BoolDecoder, tree: &[i8], probs: &[u8]) -> i32 {
    let mut i: usize = 0;
    loop {
        let b = bd.read_bool(probs[i >> 1]) as usize;
        let next = tree[i + b];
        if next <= 0 {
            return (-next) as i32;
        }
        i = next as usize;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn scan_selection_matches_libvpx_mapping() {
        // 4x4: ADST_DCT -> row scan, DCT_ADST -> col scan, others default.
        // (consts are inlined and have no stable address, so compare by value.)
        assert_eq!(get_scan(0, TxType::AdstDct).0, &ROW_SCAN_4X4[..]);
        assert_eq!(get_scan(0, TxType::DctAdst).0, &COL_SCAN_4X4[..]);
        assert_eq!(get_scan(0, TxType::DctDct).0, &DEFAULT_SCAN_4X4[..]);
        assert_eq!(get_scan(0, TxType::AdstAdst).0, &DEFAULT_SCAN_4X4[..]);
        // 32x32 ignores tx_type.
        assert_eq!(get_scan(3, TxType::AdstDct).0, &DEFAULT_SCAN_32X32[..]);
    }

    #[test]
    fn scan_tables_are_permutations() {
        for (scan, n) in [
            (&DEFAULT_SCAN_4X4[..], 16),
            (&ROW_SCAN_8X8[..], 64),
            (&COL_SCAN_16X16[..], 256),
            (&DEFAULT_SCAN_32X32[..], 1024),
        ] {
            let mut seen = vec![false; n];
            for &p in scan {
                seen[p as usize] = true;
            }
            assert!(seen.iter().all(|&b| b), "scan not a permutation");
        }
    }

    #[test]
    fn coef_context_formula() {
        // (1 + cache[nb[2c]] + cache[nb[2c+1]]) >> 1
        let nb = [0i16, 0, 1, 2];
        let cache = [4u8, 2, 5];
        assert_eq!(get_coef_context(&nb, &cache, 0), (1 + 4 + 4) >> 1); // 4
        assert_eq!(get_coef_context(&nb, &cache, 1), (1 + 2 + 5) >> 1); // 4
    }

    #[test]
    fn read_coeff_assembles_msb_first() {
        // read_coeff must equal calling read_bool n times and packing MSB-first.
        let data = [0x9Au8, 0x3C, 0x71, 0xE5, 0x42, 0x88];
        let probs = [200u8, 50, 130, 90];
        let mut a = BoolDecoder::new(&data).unwrap();
        let got = read_coeff(&mut a, &probs, 4);
        let mut b = BoolDecoder::new(&data).unwrap();
        let mut want = 0i32;
        for i in 0..4 {
            want = (want << 1) | b.read_bool(probs[i]) as i32;
        }
        assert_eq!(got, want);
    }

    #[test]
    fn read_tree_walks_to_leaf() {
        // Minimal 2-leaf tree: node 0 -> {leaf 0, leaf 1}. prob 255 ~ always 0 bit.
        let tree = [0i8, -1]; // [b=0 -> leaf 0, b=1 -> leaf 1]
        let data = [0u8; 8];
        let mut bd = BoolDecoder::new(&data).unwrap();
        // With all-zero data the first bool is 0 -> leaf 0.
        assert_eq!(read_tree(&mut bd, &tree, &[128]), 0);
    }

    #[test]
    fn decode_coefs_smoke_returns_within_bounds() {
        // Functional bounds check (exact values are proven end-to-end vs FFmpeg):
        // decode must never report more coefficients than the block holds.
        let data = [0x42u8, 0x9C, 0x17, 0xE3, 0x55, 0xAA, 0x01, 0xFF, 0x80, 0x33];
        let probs = default_coef_probs(0, 0, 0);
        let (scan, nb) = get_scan(0, TxType::DctDct);
        let mut bd = BoolDecoder::new(&data).unwrap();
        let mut out = [0i32; 16];
        let mut tc = [0u8; 16];
        let mut cc = [[[0u32; 4]; 6]; 6];
        let mut ec = [[0u32; 6]; 6];
        let (eob, _max_row) = decode_coefs(
            &mut bd,
            probs,
            0,
            scan,
            nb,
            (20, 18),
            0,
            &mut out,
            &mut tc,
            &mut cc,
            &mut ec,
            8,
        );
        assert!(eob <= 16);
    }
}
