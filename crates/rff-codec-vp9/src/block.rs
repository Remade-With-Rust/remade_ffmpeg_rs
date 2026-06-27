//! VP9 block structure & key-frame mode info (ISO/VP9 §8.4 / libvpx
//! `vp9_decodemv.c`, `vp9_decodeframe.c`). The geometry lookup tables live in
//! `geom_tables.rs` (generated + validated); this module holds the mode and
//! partition *trees* plus the deterministic primitives that drive intra
//! key-frame decoding:
//!
//! * partition decode — context (`partition_plane_context`), the per-node read
//!   (`read_partition`), `subsize`, and the context update.
//! * mode info — the above/left neighbour Y mode (`above_block_mode` /
//!   `left_block_mode`), the `kf_y_mode` / `kf_uv_mode` probability selection,
//!   the skip / tx-size contexts, and `read_intra_frame_mode_info`.
//!
//! Pure helpers are unit-tested here; the bitstream readers are exact ports
//! verified end-to-end against FFmpeg in stage H.

#![allow(dead_code)]

use crate::bits::BoolDecoder;
use crate::geom_tables::*;
use crate::prob_tables::{KF_UV_MODE_PROBS, KF_Y_MODE_PROBS};
use crate::token::read_tree;

// Block sizes.
pub const BLOCK_8X8: usize = 3;
pub const BLOCK_64X64: usize = 12;
pub const BLOCK_INVALID: u8 = 13;
// Intra prediction modes.
pub const DC_PRED: u8 = 0;
pub const TM_PRED: u8 = 9;
pub const INTRA_MODES: usize = 10;
// Partition types.
pub const PARTITION_NONE: usize = 0;
pub const PARTITION_HORZ: usize = 1;
pub const PARTITION_VERT: usize = 2;
pub const PARTITION_SPLIT: usize = 3;
// TX modes.
pub const TX_MODE_SELECT: usize = 4;
/// Mask for the left context index within a 64×64 superblock (8 mi units).
pub const MI_MASK: usize = 7;

/// `vp9_intra_mode_tree` (libvpx): non-positive entries are `-mode` leaves,
/// positive entries are the next node index; branch on `read_bool(probs[i>>1])`.
pub const INTRA_MODE_TREE: [i8; 18] = [
    0, 2, // DC / ->
    -9, 4, // TM / ->
    -1, 6, // V / ->
    8, 12, // -> / ->
    -2, 10, // H / ->
    -4, -5, // D135 / D117
    -3, 14, // D45 / ->
    -8, 16, // D63 / ->
    -6, -7, // D153 / D207
];

/// `vp9_partition_tree` (libvpx): NONE / HORZ / VERT / SPLIT.
pub const PARTITION_TREE: [i8; 6] = [0, 2, -1, 4, -2, -3];

/// A motion vector in 1/8-pel units, `(row, col)`.
pub type Mv = (i32, i32);

// Reference-frame indices (`MV_REFERENCE_FRAME`).
pub const NONE_FRAME: i8 = -1;
pub const INTRA_FRAME: i8 = 0;
pub const LAST_FRAME: i8 = 1;
pub const GOLDEN_FRAME: i8 = 2;
pub const ALTREF_FRAME: i8 = 3;
// Inter prediction modes (offset past the 10 intra modes).
pub const NEARESTMV: u8 = 10;
pub const NEARMV: u8 = 11;
pub const ZEROMV: u8 = 12;
pub const NEWMV: u8 = 13;

/// Per-block mode information (intra key-frame *and* inter).
#[derive(Clone, Copy, Debug)]
pub struct ModeInfo {
    pub sb_type: u8,    // BLOCK_SIZE
    pub mode: u8,       // block Y mode (or inter mode ≥ NEARESTMV)
    pub bmi: [u8; 4],   // sub-8×8 Y modes
    pub uv_mode: u8,    // chroma mode
    pub skip: bool,     // skip flag
    pub tx_size: u8,    // TX_SIZE 0..=3
    pub is_inter: bool,
    // ---- inter fields ----
    pub ref_frame: [i8; 2], // [NONE/INTRA/LAST/GOLDEN/ALTREF; 2]
    pub mv: [Mv; 2],        // block MVs (per reference)
    pub bmi_mv: [[Mv; 2]; 4], // sub-8×8 per-4×4 MVs
    pub interp_filter: u8,  // 0..3 or SWITCHABLE_FILTERS(3) sentinel
    pub segment_id: u8,
    pub seg_id_predicted: bool,
}

impl Default for ModeInfo {
    fn default() -> ModeInfo {
        ModeInfo {
            sb_type: BLOCK_64X64 as u8,
            mode: DC_PRED,
            bmi: [DC_PRED; 4],
            uv_mode: DC_PRED,
            skip: false,
            tx_size: 0,
            is_inter: false,
            ref_frame: [INTRA_FRAME, NONE_FRAME],
            mv: [(0, 0); 2],
            bmi_mv: [[(0, 0); 2]; 4],
            interp_filter: 3,
            segment_id: 0,
            seg_id_predicted: false,
        }
    }
}

impl ModeInfo {
    /// libvpx `get_y_mode`: sub-8×8 blocks carry per-4×4 modes in `bmi`.
    #[inline]
    pub fn get_y_mode(&self, block: usize) -> u8 {
        if (self.sb_type as usize) < BLOCK_8X8 {
            self.bmi[block]
        } else {
            self.mode
        }
    }
    /// `is_inter_block` — the first reference is a real (non-intra) frame.
    #[inline]
    pub fn is_inter_block(&self) -> bool {
        self.ref_frame[0] > INTRA_FRAME
    }
    /// `has_second_ref` — compound prediction.
    #[inline]
    pub fn has_second_ref(&self) -> bool {
        self.ref_frame[1] > INTRA_FRAME
    }
}

/// libvpx `vp9_above_block_mode`: the Y mode of the block above sub-block `b`.
pub fn above_block_mode(cur: &ModeInfo, above: Option<&ModeInfo>, b: usize) -> u8 {
    if b == 0 || b == 1 {
        match above {
            Some(a) if !a.is_inter => a.get_y_mode(b + 2),
            _ => DC_PRED,
        }
    } else {
        cur.bmi[b - 2]
    }
}

/// libvpx `vp9_left_block_mode`: the Y mode of the block left of sub-block `b`.
pub fn left_block_mode(cur: &ModeInfo, left: Option<&ModeInfo>, b: usize) -> u8 {
    if b == 0 || b == 2 {
        match left {
            Some(l) if !l.is_inter => l.get_y_mode(b + 1),
            _ => DC_PRED,
        }
    } else {
        cur.bmi[b - 1]
    }
}

/// Key-frame Y-mode tree probabilities for sub-block `b`, indexed by the above
/// and left neighbour modes (libvpx `get_y_mode_probs`).
pub fn kf_y_mode_probs(cur: &ModeInfo, above: Option<&ModeInfo>, left: Option<&ModeInfo>, b: usize) -> &'static [u8; 9] {
    let a = above_block_mode(cur, above, b) as usize;
    let l = left_block_mode(cur, left, b) as usize;
    &KF_Y_MODE_PROBS[a][l]
}

/// Key-frame UV-mode tree probabilities, indexed by the chosen Y mode.
pub fn kf_uv_mode_probs(y_mode: u8) -> &'static [u8; 9] {
    &KF_UV_MODE_PROBS[y_mode as usize]
}

/// Skip context (libvpx `vp9_get_skip_context`): sum of the neighbours' skip.
pub fn skip_context(above: Option<&ModeInfo>, left: Option<&ModeInfo>) -> usize {
    let a = above.map_or(0, |m| m.skip as usize);
    let l = left.map_or(0, |m| m.skip as usize);
    a + l
}

/// TX-size context (libvpx `get_tx_size_context`).
pub fn tx_size_context(cur: &ModeInfo, above: Option<&ModeInfo>, left: Option<&ModeInfo>) -> usize {
    let max_tx_size = MAX_TXSIZE[cur.sb_type as usize] as i32;
    let mut above_ctx = match above {
        Some(a) if !a.skip => a.tx_size as i32,
        _ => max_tx_size,
    };
    let mut left_ctx = match left {
        Some(l) if !l.skip => l.tx_size as i32,
        _ => max_tx_size,
    };
    if left.is_none() {
        left_ctx = above_ctx;
    }
    if above.is_none() {
        above_ctx = left_ctx;
    }
    ((above_ctx + left_ctx) > max_tx_size) as usize
}

/// Partition context (libvpx `dec_partition_plane_context`):
/// `(left*2 + above) + bsl*4`, where `above`/`left` are the `bsl`-th bit of the
/// above/left segment-context bytes.
pub fn partition_plane_context(above_seg: &[u8], left_seg: &[u8], mi_row: usize, mi_col: usize, bsl: usize) -> usize {
    let above = (above_seg[mi_col] >> bsl) & 1;
    let left = (left_seg[mi_row & MI_MASK] >> bsl) & 1;
    (left as usize * 2 + above as usize) + bsl * 4
}

/// Update the above/left partition (segment) context after a block of size
/// `subsize` covering `bw` mi units (libvpx `dec_update_partition_context`).
pub fn update_partition_context(above_seg: &mut [u8], left_seg: &mut [u8], mi_row: usize, mi_col: usize, subsize: usize, bw: usize) {
    let a = PARTITION_CTX_ABOVE[subsize];
    let l = PARTITION_CTX_LEFT[subsize];
    for i in 0..bw {
        above_seg[mi_col + i] = a;
        left_seg[(mi_row & MI_MASK) + i] = l;
    }
}

/// Resulting block size of `partition` applied to `bsize` (subsize_lookup).
#[inline]
pub fn subsize(partition: usize, bsize: usize) -> u8 {
    SUBSIZE_LOOKUP[partition][bsize]
}

/// Read a partition type given its context probabilities and whether the lower
/// /right halves are inside the frame (libvpx `read_partition`).
pub fn read_partition(bd: &mut BoolDecoder, probs: &[u8; 3], has_rows: bool, has_cols: bool) -> usize {
    if has_rows && has_cols {
        read_tree(bd, &PARTITION_TREE, probs) as usize
    } else if !has_rows && has_cols {
        if bd.read_bool(probs[1]) != 0 {
            PARTITION_SPLIT
        } else {
            PARTITION_HORZ
        }
    } else if has_rows && !has_cols {
        if bd.read_bool(probs[2]) != 0 {
            PARTITION_SPLIT
        } else {
            PARTITION_VERT
        }
    } else {
        PARTITION_SPLIT
    }
}

/// Read an intra prediction mode via the intra-mode tree.
pub fn read_intra_mode(bd: &mut BoolDecoder, probs: &[u8; 9]) -> u8 {
    read_tree(bd, &INTRA_MODE_TREE, probs) as u8
}

/// Read a TX size when `TX_MODE_SELECT` is active (libvpx `read_selected_tx_size`).
/// `tx_probs` holds 1/2/3 node probabilities for max TX size 8/16/32.
pub fn read_selected_tx_size(bd: &mut BoolDecoder, tx_probs: &[u8], max_tx_size: usize) -> u8 {
    let mut tx_size = bd.read_bool(tx_probs[0]) as usize;
    if tx_size != 0 && max_tx_size >= 2 {
        tx_size += bd.read_bool(tx_probs[1]) as usize;
        if tx_size != 1 && max_tx_size >= 3 {
            tx_size += bd.read_bool(tx_probs[2]) as usize;
        }
    }
    tx_size as u8
}

/// Resolve the TX size for a block (libvpx `read_tx_size`). On the intra path
/// `allow_select` is true; `tx_mode` selects the cap when not reading bits.
pub fn read_tx_size(bd: &mut BoolDecoder, bsize: usize, tx_mode: usize, allow_select: bool, tx_probs: &[u8], ctx_max_tx_probs: usize) -> u8 {
    let _ = ctx_max_tx_probs;
    let max_tx_size = MAX_TXSIZE[bsize] as usize;
    if allow_select && tx_mode == TX_MODE_SELECT && bsize >= BLOCK_8X8 {
        read_selected_tx_size(bd, tx_probs, max_tx_size)
    } else {
        max_tx_size.min(TX_MODE_TO_BIGGEST_TX[tx_mode] as usize) as u8
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A boolean tree is well-formed if walking both branches from every node
    /// eventually reaches every leaf exactly once and stays in bounds.
    fn collect_leaves(tree: &[i8], i: usize, out: &mut Vec<u8>) {
        for b in 0..2 {
            let v = tree[i + b];
            if v <= 0 {
                out.push((-v) as u8);
            } else {
                collect_leaves(tree, v as usize, out);
            }
        }
    }

    #[test]
    fn intra_mode_tree_covers_all_modes() {
        let mut leaves = vec![];
        collect_leaves(&INTRA_MODE_TREE, 0, &mut leaves);
        leaves.sort();
        assert_eq!(leaves, (0..10).collect::<Vec<u8>>());
    }

    #[test]
    fn partition_tree_covers_all_types() {
        let mut leaves = vec![];
        collect_leaves(&PARTITION_TREE, 0, &mut leaves);
        leaves.sort();
        assert_eq!(leaves, vec![0, 1, 2, 3]);
    }

    #[test]
    fn block_mode_edge_defaults_to_dc() {
        let cur = ModeInfo::default();
        // No above/left neighbour → DC for the top/left sub-blocks.
        assert_eq!(above_block_mode(&cur, None, 0), DC_PRED);
        assert_eq!(left_block_mode(&cur, None, 0), DC_PRED);
        // An inter neighbour is also treated as DC for intra mode prediction.
        let inter = ModeInfo { is_inter: true, mode: TM_PRED, ..Default::default() };
        assert_eq!(above_block_mode(&cur, Some(&inter), 1), DC_PRED);
    }

    #[test]
    fn block_mode_reads_neighbour_and_self() {
        // 8×8 above neighbour with mode V → above mode of top sub-blocks is V.
        let above = ModeInfo { sb_type: BLOCK_8X8 as u8, mode: 1, ..Default::default() };
        let cur = ModeInfo { sb_type: BLOCK_8X8 as u8, bmi: [3, 4, 5, 6], ..Default::default() };
        assert_eq!(above_block_mode(&cur, Some(&above), 0), 1);
        // Lower sub-blocks (b=2,3) read the current block's own bmi.
        assert_eq!(above_block_mode(&cur, Some(&above), 2), cur.bmi[0]);
        assert_eq!(left_block_mode(&cur, None, 1), cur.bmi[0]);
    }

    #[test]
    fn kf_mode_prob_selection_indexes_by_neighbours() {
        let above = ModeInfo { sb_type: BLOCK_8X8 as u8, mode: 2, ..Default::default() };
        let left = ModeInfo { sb_type: BLOCK_8X8 as u8, mode: 5, ..Default::default() };
        let cur = ModeInfo { sb_type: BLOCK_8X8 as u8, ..Default::default() };
        let p = kf_y_mode_probs(&cur, Some(&above), Some(&left), 0);
        assert_eq!(p, &KF_Y_MODE_PROBS[2][5]);
    }

    #[test]
    fn contexts_match_formulas() {
        let s0 = ModeInfo { skip: false, ..Default::default() };
        let s1 = ModeInfo { skip: true, ..Default::default() };
        assert_eq!(skip_context(Some(&s1), Some(&s1)), 2);
        assert_eq!(skip_context(Some(&s0), None), 0);
        // partition context: bsl picks the bit, packs (left*2+above)+bsl*4.
        let above_seg = [0b1000u8; 8];
        let left_seg = [0b1000u8; 8];
        assert_eq!(partition_plane_context(&above_seg, &left_seg, 0, 0, 3), (1 * 2 + 1) + 3 * 4);
        assert_eq!(partition_plane_context(&above_seg, &left_seg, 0, 0, 2), 0 + 2 * 4);
    }

    #[test]
    fn subsize_and_partition_ctx_update() {
        // SPLIT of 64×64 → 32×32 (index 9).
        assert_eq!(subsize(PARTITION_SPLIT, BLOCK_64X64), 9);
        // NONE keeps the block size.
        assert_eq!(subsize(PARTITION_NONE, BLOCK_64X64), BLOCK_64X64 as u8);
        let mut a = [0u8; 8];
        let mut l = [0u8; 8];
        update_partition_context(&mut a, &mut l, 0, 0, BLOCK_8X8, 1);
        assert_eq!(a[0], PARTITION_CTX_ABOVE[BLOCK_8X8]);
        assert_eq!(l[0], PARTITION_CTX_LEFT[BLOCK_8X8]);
    }
}
