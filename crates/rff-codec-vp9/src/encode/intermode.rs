//! VP9 encoder — inter mode-info field serializers (Floor 2, brick B5).
//!
//! Each writer is the exact inverse of a decoder inter-block reader, reusing the
//! same trees verbatim and gated by a round-trip *through that reader*. As with
//! the intra serializers (B3), the callers supply the context-selected
//! probabilities; the full neighbour-context assembly (ref-frame contexts,
//! sub-8×8 blocks, MV prediction via `find_mv_refs` + B4's `encode_mv`) is the
//! inter encode loop (plan Floor 4), which threads these field writers together.

use super::bitwriter::BoolEncoder;
use crate::block::{ALTREF_FRAME, LAST_FRAME, NEARESTMV};
use crate::decode::INTER_MODE_TREE;

/// Inverse of [`read_inter_mode`](crate::decode::read_inter_mode): write the
/// inter mode (`NEARESTMV..=NEWMV`).
pub fn write_inter_mode(enc: &mut BoolEncoder, mode: u8, probs: &[u8; 3]) {
    enc.write_tree(&INTER_MODE_TREE, probs, (mode - NEARESTMV) as i32);
}

/// SWITCHABLE interpolation-filter tree (EIGHTTAP / EIGHTTAP_SMOOTH / EIGHTTAP_SHARP).
const SWITCHABLE_INTERP_TREE: [i8; 4] = [0, 2, -1, -2];

/// Inverse of `read_switchable_interp_filter`: write the filter (0..=2).
pub fn write_interp_filter(enc: &mut BoolEncoder, filter: u8, probs: &[u8; 2]) {
    enc.write_tree(&SWITCHABLE_INTERP_TREE, probs, filter as i32);
}

/// Inverse of the intra-vs-inter block flag (`is_inter`).
pub fn write_is_inter(enc: &mut BoolEncoder, is_inter: bool, prob: u8) {
    enc.write_bool(is_inter as u32, prob);
}

/// Inverse of the single-vs-compound reference-mode bit (REFERENCE_MODE_SELECT).
pub fn write_comp_inter(enc: &mut BoolEncoder, is_compound: bool, prob: u8) {
    enc.write_bool(is_compound as u32, prob);
}

/// Inverse of the single-reference selection — LAST / GOLDEN / ALTREF. `p1` is
/// the first-bit prob (`single_ref_prob[ctx0][0]`), `p2` the second
/// (`single_ref_prob[ctx1][1]`).
pub fn write_single_ref(enc: &mut BoolEncoder, ref_frame: i8, p1: u8, p2: u8) {
    if ref_frame == LAST_FRAME {
        enc.write_bool(0, p1);
    } else {
        enc.write_bool(1, p1);
        enc.write_bool((ref_frame == ALTREF_FRAME) as u32, p2);
    }
}

/// Inverse of the compound variable-reference bit (selecting `comp_var_ref[bit]`).
pub fn write_comp_ref(enc: &mut BoolEncoder, var_bit: u32, prob: u8) {
    enc.write_bool(var_bit, prob);
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::bits::BoolDecoder;
    use crate::block::{GOLDEN_FRAME, NEWMV};
    use crate::decode::read_inter_mode;
    use crate::token::read_tree;

    #[test]
    fn inter_mode_roundtrips() {
        let probs = [90u8, 140, 60];
        for mode in NEARESTMV..=NEWMV {
            let mut enc = BoolEncoder::new();
            write_inter_mode(&mut enc, mode, &probs);
            let bytes = enc.finish();
            let mut bd = BoolDecoder::new(&bytes).unwrap();
            assert_eq!(read_inter_mode(&mut bd, &probs), mode);
        }
    }

    #[test]
    fn interp_filter_roundtrips() {
        let probs = [110u8, 70];
        for filter in 0..3u8 {
            let mut enc = BoolEncoder::new();
            write_interp_filter(&mut enc, filter, &probs);
            let bytes = enc.finish();
            let mut bd = BoolDecoder::new(&bytes).unwrap();
            assert_eq!(
                read_tree(&mut bd, &SWITCHABLE_INTERP_TREE, &probs) as u8,
                filter
            );
        }
    }

    #[test]
    fn single_ref_roundtrips() {
        let (p1, p2) = (100u8, 150u8);
        for rf in [LAST_FRAME, GOLDEN_FRAME, ALTREF_FRAME] {
            let mut enc = BoolEncoder::new();
            write_single_ref(&mut enc, rf, p1, p2);
            let bytes = enc.finish();
            let mut bd = BoolDecoder::new(&bytes).unwrap();
            // Replicate the decoder's single-ref read.
            let got = if bd.read_bool(p1) != 0 {
                if bd.read_bool(p2) != 0 {
                    ALTREF_FRAME
                } else {
                    GOLDEN_FRAME
                }
            } else {
                LAST_FRAME
            };
            assert_eq!(got, rf);
        }
    }

    #[test]
    fn ref_mode_and_flag_bits_roundtrip() {
        for prob in [1u8, 50, 128, 200, 255] {
            for val in [false, true] {
                // is_inter / comp_inter / comp_ref are all single bools.
                let mut enc = BoolEncoder::new();
                write_is_inter(&mut enc, val, prob);
                write_comp_inter(&mut enc, val, prob);
                write_comp_ref(&mut enc, val as u32, prob);
                let bytes = enc.finish();
                let mut bd = BoolDecoder::new(&bytes).unwrap();
                assert_eq!(bd.read_bool(prob) != 0, val);
                assert_eq!(bd.read_bool(prob) != 0, val);
                assert_eq!(bd.read_bool(prob), val as u32);
            }
        }
    }
}
