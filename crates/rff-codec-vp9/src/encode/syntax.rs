//! VP9 encoder — mode-info serializers (Floor 2, brick B3: intra key-frame).
//!
//! Each writer is the exact inverse of a decoder mode-info reader in
//! [`crate::block`] / [`crate::decode`], reusing the same trees verbatim and
//! gated by a round-trip *through that reader*. The inter-side serializers
//! (B4 MV, B5 inter mode-info) build on these.

use super::bitwriter::BoolEncoder;
use crate::block::{INTRA_MODE_TREE, PARTITION_SPLIT, PARTITION_TREE};
use crate::decode::SEGMENT_TREE;

/// Inverse of [`read_partition`](crate::block::read_partition). Writes the
/// partition decision, honouring the frame-edge cases where only HORZ/SPLIT
/// (no rows) or VERT/SPLIT (no cols) are codable, and the forced SPLIT (neither)
/// which writes no bits.
pub fn write_partition(
    enc: &mut BoolEncoder,
    partition: usize,
    probs: &[u8; 3],
    has_rows: bool,
    has_cols: bool,
) {
    if has_rows && has_cols {
        enc.write_tree(&PARTITION_TREE, probs, partition as i32);
    } else if !has_rows && has_cols {
        enc.write_bool((partition == PARTITION_SPLIT) as u32, probs[1]);
    } else if has_rows && !has_cols {
        enc.write_bool((partition == PARTITION_SPLIT) as u32, probs[2]);
    }
    // Neither rows nor cols inside the frame: SPLIT is forced, no bits written.
}

/// Inverse of [`read_intra_mode`](crate::block::read_intra_mode). Works for both
/// the key-frame Y mode (with `kf_y_mode_probs` context) and the UV mode (with
/// `kf_uv_mode_probs`) — the caller supplies the context-selected probabilities.
pub fn write_intra_mode(enc: &mut BoolEncoder, mode: u8, probs: &[u8; 9]) {
    enc.write_tree(&INTRA_MODE_TREE, probs, mode as i32);
}

/// Inverse of [`read_selected_tx_size`](crate::block::read_selected_tx_size):
/// the variable-depth TX-size tree (1..3 bits depending on `max_tx_size`).
pub fn write_selected_tx_size(
    enc: &mut BoolEncoder,
    tx_size: u8,
    tx_probs: &[u8],
    max_tx_size: usize,
) {
    let t = tx_size as usize;
    enc.write_bool((t >= 1) as u32, tx_probs[0]);
    if t >= 1 && max_tx_size >= 2 {
        enc.write_bool((t >= 2) as u32, tx_probs[1]);
        if t >= 2 && max_tx_size >= 3 {
            enc.write_bool((t >= 3) as u32, tx_probs[2]);
        }
    }
}

/// Inverse of the skip-flag read — a single boolean at `prob`.
pub fn write_skip(enc: &mut BoolEncoder, skip: bool, prob: u8) {
    enc.write_bool(skip as u32, prob);
}

/// Inverse of [`read_segment_id_tree`](crate::decode) — the 8-leaf segment tree.
pub fn write_segment_id(enc: &mut BoolEncoder, seg_id: u8, tree_probs: &[u8; 7]) {
    enc.write_tree(&SEGMENT_TREE, tree_probs, seg_id as i32);
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::bits::BoolDecoder;
    use crate::block::{
        read_intra_mode, read_partition, read_selected_tx_size, PARTITION_HORZ, PARTITION_VERT,
    };
    use crate::token::read_tree;

    #[test]
    fn partition_roundtrips_including_edges() {
        let probs = [120u8, 90, 200];
        // Interior: all four partitions through the full tree.
        for part in 0..4usize {
            let mut enc = BoolEncoder::new();
            write_partition(&mut enc, part, &probs, true, true);
            let bytes = enc.finish();
            let mut bd = BoolDecoder::new(&bytes).unwrap();
            assert_eq!(read_partition(&mut bd, &probs, true, true), part);
        }
        // Bottom edge (no rows): HORZ or SPLIT only.
        for part in [PARTITION_HORZ, PARTITION_SPLIT] {
            let mut enc = BoolEncoder::new();
            write_partition(&mut enc, part, &probs, false, true);
            let bytes = enc.finish();
            let mut bd = BoolDecoder::new(&bytes).unwrap();
            assert_eq!(read_partition(&mut bd, &probs, false, true), part);
        }
        // Right edge (no cols): VERT or SPLIT only.
        for part in [PARTITION_VERT, PARTITION_SPLIT] {
            let mut enc = BoolEncoder::new();
            write_partition(&mut enc, part, &probs, true, false);
            let bytes = enc.finish();
            let mut bd = BoolDecoder::new(&bytes).unwrap();
            assert_eq!(read_partition(&mut bd, &probs, true, false), part);
        }
        // Corner (neither): SPLIT forced, no bits.
        let mut enc = BoolEncoder::new();
        write_partition(&mut enc, PARTITION_SPLIT, &probs, false, false);
        let bytes = enc.finish();
        let mut bd = BoolDecoder::new(&bytes).unwrap();
        assert_eq!(
            read_partition(&mut bd, &probs, false, false),
            PARTITION_SPLIT
        );
    }

    #[test]
    fn intra_mode_roundtrips_all_modes() {
        let probs = [80u8, 100, 120, 140, 90, 110, 130, 150, 70];
        for mode in 0..10u8 {
            let mut enc = BoolEncoder::new();
            write_intra_mode(&mut enc, mode, &probs);
            let bytes = enc.finish();
            let mut bd = BoolDecoder::new(&bytes).unwrap();
            assert_eq!(read_intra_mode(&mut bd, &probs), mode);
        }
    }

    #[test]
    fn tx_size_roundtrips_every_depth() {
        let tx_probs = [100u8, 130, 160];
        for max in 1..=3usize {
            for tx in 0..=max as u8 {
                let mut enc = BoolEncoder::new();
                write_selected_tx_size(&mut enc, tx, &tx_probs, max);
                let bytes = enc.finish();
                let mut bd = BoolDecoder::new(&bytes).unwrap();
                assert_eq!(read_selected_tx_size(&mut bd, &tx_probs, max), tx);
            }
        }
    }

    #[test]
    fn segment_id_roundtrips() {
        let tree_probs = [120u8, 90, 200, 100, 130, 160, 80];
        for seg in 0..8u8 {
            let mut enc = BoolEncoder::new();
            write_segment_id(&mut enc, seg, &tree_probs);
            let bytes = enc.finish();
            let mut bd = BoolDecoder::new(&bytes).unwrap();
            assert_eq!(read_tree(&mut bd, &SEGMENT_TREE, &tree_probs) as u8, seg);
        }
    }

    #[test]
    fn skip_roundtrips() {
        for skip in [false, true] {
            for prob in [1u8, 64, 128, 200, 255] {
                let mut enc = BoolEncoder::new();
                write_skip(&mut enc, skip, prob);
                let bytes = enc.finish();
                let mut bd = BoolDecoder::new(&bytes).unwrap();
                assert_eq!(bd.read_bool(prob) != 0, skip);
            }
        }
    }
}
