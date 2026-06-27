//! VP9 key-frame reconstruction (ISO/VP9 §8 / libvpx `vp9_decodeframe.c`). This
//! is where every primitive converges: the compressed-header probability
//! updates build a [`FrameContext`]; the tile boolean decoder drives the
//! `decode_partition` recursion; each coding block reads its mode info, then per
//! plane and transform block runs intra prediction → token decode → dequant →
//! inverse transform → add. The output is a planar 4:4:4 / 4:2:0 frame.
//!
//! Targeted at the 8-bit intra key-frame path (Profiles 0/1). Inter prediction
//! and the loop filter are later stages.

use crate::bits::BoolDecoder;
use crate::block::{
    kf_uv_mode_probs, kf_y_mode_probs, partition_plane_context, read_intra_mode, read_partition,
    read_selected_tx_size, skip_context, subsize, tx_size_context, update_partition_context,
    ModeInfo, BLOCK_8X8, PARTITION_HORZ, PARTITION_NONE, PARTITION_SPLIT, PARTITION_VERT,
};
use crate::block::{
    ALTREF_FRAME, GOLDEN_FRAME, INTRA_FRAME, LAST_FRAME, Mv, NEARESTMV, NEARMV, NEWMV, NONE_FRAME,
    ZEROMV,
};
use crate::geom_tables::{
    B_HEIGHT_LOG2, B_WIDTH_LOG2, MAX_TXSIZE, SIZE_GROUP, TX_MODE_TO_BIGGEST_TX,
};
use crate::inter::{predict_block, scaled_predict_block, RefPlane};
use crate::mv::{find_mv_refs, get_mode_context, lower_mv_precision, read_mv, MvRef};

/// `vp9_inter_mode_tree` — leaves are `-INTER_OFFSET(mode)`; result + NEARESTMV.
const INTER_MODE_TREE: [i8; 6] = [-2, 2, 0, 4, -1, -3];
use crate::predict::{build_intra_edges, predict};
use crate::prob::inv_remap_prob;
use crate::prob_tables::{
    DEFAULT_COEF_PROBS, DEFAULT_COMP_INTER_P, DEFAULT_COMP_REF_P, DEFAULT_IF_UV_PROBS,
    DEFAULT_IF_Y_PROBS, DEFAULT_INTER_MODE_PROBS, DEFAULT_INTRA_INTER_P, DEFAULT_NMV_CONTEXT,
    DEFAULT_PARTITION_PROBS, DEFAULT_SINGLE_REF_P, DEFAULT_SKIP_PROB, DEFAULT_SWITCHABLE_INTERP_PROB,
    KF_PARTITION_PROBS, NmvContext,
};
use crate::quant::Dequant;
use crate::token::{decode_coefs, get_scan};
use crate::transform::{inverse_transform_add_rows, inverse_transform_dc_add, inverse_wht_add, TxType, INTRA_MODE_TO_TX_TYPE};
use crate::FrameHeader;

const TX_MODE_SELECT: usize = 4;
const MI_SIZE: usize = 8; // pixels per mode-info unit

// ---- frame context (probabilities) --------------------------------------

/// The full entropy state for a frame (`FRAME_CONTEXT`): coefficient model
/// probs, skip / tx-size probs, and all inter-frame mode/reference/mv probs.
/// Persisted across frames (the 4 saved contexts) and adapted backward.
#[derive(Clone)]
pub(crate) struct FrameContext {
    coef_probs: [[[[[[u8; 3]; 6]; 6]; 2]; 2]; 4],
    skip_probs: [u8; 3],
    tx_p8x8: [[u8; 1]; 2],
    tx_p16x16: [[u8; 2]; 2],
    tx_p32x32: [[u8; 3]; 2],
    tx_mode: usize,
    // ---- inter-frame probabilities ----
    y_mode_prob: [[u8; 9]; 4],
    uv_mode_prob: [[u8; 9]; 10],
    partition_prob: [[u8; 3]; 16],
    inter_mode_probs: [[u8; 3]; 7],
    intra_inter_prob: [u8; 4],
    comp_inter_prob: [u8; 5],
    comp_ref_prob: [u8; 5],
    single_ref_prob: [[u8; 2]; 5],
    switchable_interp_prob: [[u8; 2]; 4],
    nmvc: NmvContext,
    /// 0 = SINGLE_REFERENCE, 1 = COMPOUND_REFERENCE, 2 = REFERENCE_MODE_SELECT.
    reference_mode: usize,
    /// Resolved fixed/var compound references (`vp9_setup_compound_reference_mode`).
    comp_fixed_ref: usize,
    comp_var_ref: [usize; 2],
}

impl Default for FrameContext {
    fn default() -> FrameContext {
        FrameContext::defaults()
    }
}

impl FrameContext {
    pub(crate) fn defaults() -> FrameContext {
        FrameContext {
            coef_probs: DEFAULT_COEF_PROBS,
            skip_probs: DEFAULT_SKIP_PROB,
            // libvpx default_tx_probs (p8x8/p16x16/p32x32).
            tx_p8x8: [[100], [66]],
            tx_p16x16: [[20, 152], [15, 101]],
            tx_p32x32: [[3, 136, 37], [5, 52, 13]],
            tx_mode: 0,
            y_mode_prob: DEFAULT_IF_Y_PROBS,
            uv_mode_prob: DEFAULT_IF_UV_PROBS,
            partition_prob: DEFAULT_PARTITION_PROBS,
            inter_mode_probs: DEFAULT_INTER_MODE_PROBS,
            intra_inter_prob: DEFAULT_INTRA_INTER_P,
            comp_inter_prob: DEFAULT_COMP_INTER_P,
            comp_ref_prob: DEFAULT_COMP_REF_P,
            single_ref_prob: DEFAULT_SINGLE_REF_P,
            switchable_interp_prob: DEFAULT_SWITCHABLE_INTERP_PROB,
            nmvc: DEFAULT_NMV_CONTEXT,
            reference_mode: 0,
            comp_fixed_ref: 0,
            comp_var_ref: [0, 1],
        }
    }
}

// ---- Primitive 1.2: per-frame symbol counts -----------------------------

use crate::mv::NmvCounts;

/// All symbol counts accumulated while decoding one frame (`FRAME_COUNTS`).
#[derive(Clone)]
struct FrameCounts {
    /// `coef[tx][plane][ref][band][ctx][ZERO|ONE|TWO|EOB_MODEL]`.
    coef: Box<[[[[[[u32; 4]; 6]; 6]; 2]; 2]; 4]>,
    /// `eob_branch[tx][plane][ref][band][ctx]`.
    eob_branch: Box<[[[[[u32; 6]; 6]; 2]; 2]; 4]>,
    intra_inter: [[u32; 2]; 4],
    comp_inter: [[u32; 2]; 5],
    comp_ref: [[u32; 2]; 5],
    single_ref: [[[u32; 2]; 2]; 5],
    inter_mode: [[u32; 4]; 7],
    y_mode: [[u32; 10]; 4],
    uv_mode: [[u32; 10]; 10],
    partition: [[u32; 4]; 16],
    switchable_interp: [[u32; 3]; 4],
    skip: [[u32; 2]; 3],
    tx_p8x8: [[u32; 2]; 2],
    tx_p16x16: [[u32; 3]; 2],
    tx_p32x32: [[u32; 4]; 2],
    mv: NmvCounts,
}

impl FrameCounts {
    fn zeroed() -> FrameCounts {
        FrameCounts {
            coef: Box::new([[[[[[0; 4]; 6]; 6]; 2]; 2]; 4]),
            eob_branch: Box::new([[[[[0; 6]; 6]; 2]; 2]; 4]),
            intra_inter: Default::default(),
            comp_inter: Default::default(),
            comp_ref: Default::default(),
            single_ref: Default::default(),
            inter_mode: Default::default(),
            y_mode: Default::default(),
            uv_mode: Default::default(),
            partition: Default::default(),
            switchable_interp: Default::default(),
            skip: Default::default(),
            tx_p8x8: Default::default(),
            tx_p16x16: Default::default(),
            tx_p32x32: Default::default(),
            mv: Default::default(),
        }
    }
}

// ---- Primitives 1.4/1.5/1.6: backward probability adaptation -------------

const INTRA_MODE_TREE: [i8; 18] = [
    0, 2, -9, 4, -1, 6, 8, 12, -2, 10, -4, -5, -3, 14, -8, 16, -6, -7,
];
const PARTITION_TREE: [i8; 6] = [0, 2, -1, 4, -2, -3];
const MV_JOINT_TREE: [i8; 6] = [0, 2, -1, 4, -2, -3];
const MV_CLASS_TREE: [i8; 20] = [
    0, 2, -1, 4, 6, 8, -2, -3, 10, 12, -4, -5, -6, 14, 16, 18, -7, -8, -9, -10,
];
const MV_CLASS0_TREE: [i8; 2] = [0, -1];
const MV_FP_TREE: [i8; 6] = [0, 2, -1, 4, -2, -3];
const SWITCHABLE_TREE: [i8; 4] = [0, 2, -1, -2];

/// `vp9_adapt_coef_probs` — merge coefficient model probs from token counts.
fn adapt_coef_probs(fc: &mut FrameContext, pre: &FrameContext, counts: &FrameCounts, count_sat: u32, update_factor: u32) {
    use crate::adapt::merge_probs;
    for tx in 0..4 {
        for i in 0..2 {
            for j in 0..2 {
                for k in 0..6 {
                    let nctx = if k == 0 { 3 } else { 6 };
                    for l in 0..nctx {
                        let c = &counts.coef[tx][i][j][k][l];
                        let (n0, n1, n2, neob) = (c[0], c[1], c[2], c[3]);
                        let eob = counts.eob_branch[tx][i][j][k][l];
                        let branch = [[neob, eob - neob], [n0, n1 + n2], [n1, n2]];
                        for m in 0..3 {
                            fc.coef_probs[tx][i][j][k][l][m] =
                                merge_probs(pre.coef_probs[tx][i][j][k][l][m], branch[m], count_sat, update_factor);
                        }
                    }
                }
            }
        }
    }
}

/// `vp9_adapt_mode_probs`.
fn adapt_mode_probs(fc: &mut FrameContext, pre: &FrameContext, counts: &FrameCounts, interp_switchable: bool, tx_select: bool) {
    use crate::adapt::{mode_mv_merge_probs as mm, tree_merge_probs};
    for i in 0..4 {
        fc.intra_inter_prob[i] = mm(pre.intra_inter_prob[i], counts.intra_inter[i]);
    }
    for i in 0..5 {
        fc.comp_inter_prob[i] = mm(pre.comp_inter_prob[i], counts.comp_inter[i]);
        fc.comp_ref_prob[i] = mm(pre.comp_ref_prob[i], counts.comp_ref[i]);
        for j in 0..2 {
            fc.single_ref_prob[i][j] = mm(pre.single_ref_prob[i][j], counts.single_ref[i][j]);
        }
    }
    for i in 0..7 {
        tree_merge_probs(&INTER_MODE_TREE, &pre.inter_mode_probs[i], &counts.inter_mode[i], &mut fc.inter_mode_probs[i]);
    }
    for i in 0..4 {
        tree_merge_probs(&INTRA_MODE_TREE, &pre.y_mode_prob[i], &counts.y_mode[i], &mut fc.y_mode_prob[i]);
    }
    for i in 0..10 {
        tree_merge_probs(&INTRA_MODE_TREE, &pre.uv_mode_prob[i], &counts.uv_mode[i], &mut fc.uv_mode_prob[i]);
    }
    for i in 0..16 {
        tree_merge_probs(&PARTITION_TREE, &pre.partition_prob[i], &counts.partition[i], &mut fc.partition_prob[i]);
    }
    if interp_switchable {
        for i in 0..4 {
            tree_merge_probs(&SWITCHABLE_TREE, &pre.switchable_interp_prob[i], &counts.switchable_interp[i], &mut fc.switchable_interp_prob[i]);
        }
    }
    if tx_select {
        for i in 0..2 {
            // 8x8: branch [c4x4, c8x8].
            let p = &counts.tx_p8x8[i];
            fc.tx_p8x8[i][0] = mm(pre.tx_p8x8[i][0], [p[0], p[1]]);
            // 16x16.
            let p = &counts.tx_p16x16[i];
            let b16 = [[p[0], p[1] + p[2]], [p[1], p[2]]];
            for j in 0..2 {
                fc.tx_p16x16[i][j] = mm(pre.tx_p16x16[i][j], b16[j]);
            }
            // 32x32.
            let p = &counts.tx_p32x32[i];
            let b32 = [[p[0], p[1] + p[2] + p[3]], [p[1], p[2] + p[3]], [p[2], p[3]]];
            for j in 0..3 {
                fc.tx_p32x32[i][j] = mm(pre.tx_p32x32[i][j], b32[j]);
            }
        }
    }
    for i in 0..3 {
        fc.skip_probs[i] = mm(pre.skip_probs[i], counts.skip[i]);
    }
}

/// `vp9_adapt_mv_probs`.
fn adapt_mv_probs(fc: &mut FrameContext, pre: &FrameContext, counts: &FrameCounts, allow_hp: bool) {
    use crate::adapt::{mode_mv_merge_probs as mm, tree_merge_probs};
    tree_merge_probs(&MV_JOINT_TREE, &pre.nmvc.joints, &counts.mv.joints, &mut fc.nmvc.joints);
    for i in 0..2 {
        let (fco, pco, cco) = (&mut fc.nmvc.comps[i], &pre.nmvc.comps[i], &counts.mv.comps[i]);
        fco.sign = mm(pco.sign, cco.sign);
        tree_merge_probs(&MV_CLASS_TREE, &pco.classes, &cco.classes, &mut fco.classes);
        tree_merge_probs(&MV_CLASS0_TREE, &pco.class0, &cco.class0, &mut fco.class0);
        for j in 0..10 {
            fco.bits[j] = mm(pco.bits[j], cco.bits[j]);
        }
        for j in 0..2 {
            tree_merge_probs(&MV_FP_TREE, &pco.class0_fp[j], &cco.class0_fp[j], &mut fco.class0_fp[j]);
        }
        tree_merge_probs(&MV_FP_TREE, &pco.fp, &cco.fp, &mut fco.fp);
        if allow_hp {
            fco.class0_hp = mm(pco.class0_hp, cco.class0_hp);
            fco.hp = mm(pco.hp, cco.hp);
        }
    }
}

/// `vp9_diff_update_prob` over a whole slice.
fn diff_update_slice(b: &mut BoolDecoder, p: &mut [u8]) {
    for v in p.iter_mut() {
        diff_update(b, v);
    }
}

/// `update_mv_probs` — the MV-specific prob update (read(252) then literal(7)).
fn update_mv_probs(b: &mut BoolDecoder, p: &mut [u8]) {
    for v in p.iter_mut() {
        if b.read_bool(252) == 1 {
            *v = ((b.literal(7) << 1) | 1) as u8;
        }
    }
}

/// `read_inter_mode` — the inter mode (NEARESTMV..NEWMV) for one block.
fn read_inter_mode(b: &mut BoolDecoder, probs: &[u8; 3]) -> u8 {
    NEARESTMV + crate::token::read_tree(b, &INTER_MODE_TREE, probs) as u8
}

/// `clamp_mv_to_umv_border_sb` — scale an MV to the plane's 1/16-pel grid and
/// clamp it so the reference access stays within the unrestricted-MV border.
fn clamp_mv_umv(mv: Mv, bw: i32, bh: i32, ss_x: usize, ss_y: usize, edges: (i32, i32, i32, i32)) -> Mv {
    let spel_left = (4 + bw) << 4;
    let spel_right = spel_left - 16;
    let spel_top = (4 + bh) << 4;
    let spel_bottom = spel_top - 16;
    let sx = 1 << (1 - ss_x);
    let sy = 1 << (1 - ss_y);
    let row = (mv.0 * sy).clamp(edges.2 * sy - spel_top, edges.3 * sy + spel_bottom);
    let col = (mv.1 * sx).clamp(edges.0 * sx - spel_left, edges.1 * sx + spel_right);
    (row, col)
}

#[inline]
fn round_q2(v: i32) -> i32 {
    (if v < 0 { v - 1 } else { v + 1 }) / 2
}
#[inline]
fn round_q4(v: i32) -> i32 {
    (if v < 0 { v - 2 } else { v + 2 }) / 4
}

/// `average_split_mvs` — the MV for a plane's 4×4 sub-block, combining the
/// sub-8×8 per-block MVs according to chroma subsampling.
fn average_split_mvs(mi: &ModeInfo, r: usize, block: usize, ss_x: usize, ss_y: usize) -> Mv {
    let q2 = |b0: usize, b1: usize| {
        (
            round_q2(mi.bmi_mv[b0][r].0 + mi.bmi_mv[b1][r].0),
            round_q2(mi.bmi_mv[b0][r].1 + mi.bmi_mv[b1][r].1),
        )
    };
    match (((ss_x > 0) as usize) << 1) | (ss_y > 0) as usize {
        0 => mi.bmi_mv[block][r],
        1 => q2(block, block + 2),
        2 => q2(block, block + 1),
        _ => {
            let (mut sr, mut sc) = (0i32, 0i32);
            for k in 0..4 {
                sr += mi.bmi_mv[k][r].0;
                sc += mi.bmi_mv[k][r].1;
            }
            (round_q4(sr), round_q4(sc))
        }
    }
}

// ---- inter prediction contexts (libvpx vp9_pred_common.c) ---------------

fn intra_inter_context(above: Option<&ModeInfo>, left: Option<&ModeInfo>) -> usize {
    match (above, left) {
        (Some(a), Some(l)) => {
            let ai = !a.is_inter_block();
            let li = !l.is_inter_block();
            if li && ai {
                3
            } else {
                (li || ai) as usize
            }
        }
        (Some(m), None) | (None, Some(m)) => 2 * (!m.is_inter_block()) as usize,
        (None, None) => 0,
    }
}

fn switchable_interp_context(above: Option<&ModeInfo>, left: Option<&ModeInfo>) -> usize {
    const SW: usize = 3; // SWITCHABLE_FILTERS
    let left_type = left.map_or(SW, |m| m.interp_filter as usize);
    let above_type = above.map_or(SW, |m| m.interp_filter as usize);
    if left_type == above_type {
        left_type
    } else if left_type == SW {
        above_type
    } else if above_type == SW {
        left_type
    } else {
        SW
    }
}

fn single_ref_p1(above: Option<&ModeInfo>, left: Option<&ModeInfo>) -> usize {
    let last = |m: &ModeInfo| {
        if !m.has_second_ref() {
            4 * (m.ref_frame[0] == LAST_FRAME) as usize
        } else {
            1 + (m.ref_frame[0] == LAST_FRAME || m.ref_frame[1] == LAST_FRAME) as usize
        }
    };
    match (above, left) {
        (Some(a), Some(l)) => {
            let ai = !a.is_inter_block();
            let li = !l.is_inter_block();
            if ai && li {
                2
            } else if ai || li {
                last(if ai { l } else { a })
            } else {
                let (ah, lh) = (a.has_second_ref(), l.has_second_ref());
                let (a0, a1, l0, l1) = (a.ref_frame[0], a.ref_frame[1], l.ref_frame[0], l.ref_frame[1]);
                if ah && lh {
                    1 + (a0 == LAST_FRAME || a1 == LAST_FRAME || l0 == LAST_FRAME || l1 == LAST_FRAME) as usize
                } else if ah || lh {
                    let rfs = if !ah { a0 } else { l0 };
                    let crf1 = if ah { a0 } else { l0 };
                    let crf2 = if ah { a1 } else { l1 };
                    if rfs == LAST_FRAME {
                        3 + (crf1 == LAST_FRAME || crf2 == LAST_FRAME) as usize
                    } else {
                        (crf1 == LAST_FRAME || crf2 == LAST_FRAME) as usize
                    }
                } else {
                    2 * (a0 == LAST_FRAME) as usize + 2 * (l0 == LAST_FRAME) as usize
                }
            }
        }
        (Some(m), None) | (None, Some(m)) => {
            if !m.is_inter_block() {
                2
            } else {
                last(m)
            }
        }
        (None, None) => 2,
    }
}

fn single_ref_p2(above: Option<&ModeInfo>, left: Option<&ModeInfo>) -> usize {
    let edge = |m: &ModeInfo| {
        if !m.has_second_ref() {
            if m.ref_frame[0] == LAST_FRAME {
                3
            } else {
                4 * (m.ref_frame[0] == GOLDEN_FRAME) as usize
            }
        } else {
            1 + 2 * (m.ref_frame[0] == GOLDEN_FRAME || m.ref_frame[1] == GOLDEN_FRAME) as usize
        }
    };
    match (above, left) {
        (Some(a), Some(l)) => {
            let ai = !a.is_inter_block();
            let li = !l.is_inter_block();
            if ai && li {
                2
            } else if ai || li {
                edge(if ai { l } else { a })
            } else {
                let (ah, lh) = (a.has_second_ref(), l.has_second_ref());
                let (a0, a1, l0, l1) = (a.ref_frame[0], a.ref_frame[1], l.ref_frame[0], l.ref_frame[1]);
                if ah && lh {
                    if a0 == l0 && a1 == l1 {
                        3 * (a0 == GOLDEN_FRAME || a1 == GOLDEN_FRAME || l0 == GOLDEN_FRAME || l1 == GOLDEN_FRAME) as usize
                    } else {
                        2
                    }
                } else if ah || lh {
                    let rfs = if !ah { a0 } else { l0 };
                    let crf1 = if ah { a0 } else { l0 };
                    let crf2 = if ah { a1 } else { l1 };
                    if rfs == GOLDEN_FRAME {
                        3 + (crf1 == GOLDEN_FRAME || crf2 == GOLDEN_FRAME) as usize
                    } else if rfs == ALTREF_FRAME {
                        (crf1 == GOLDEN_FRAME || crf2 == GOLDEN_FRAME) as usize
                    } else {
                        1 + 2 * (crf1 == GOLDEN_FRAME || crf2 == GOLDEN_FRAME) as usize
                    }
                } else if a0 == LAST_FRAME && l0 == LAST_FRAME {
                    3
                } else if a0 == LAST_FRAME || l0 == LAST_FRAME {
                    let edge0 = if a0 == LAST_FRAME { l0 } else { a0 };
                    4 * (edge0 == GOLDEN_FRAME) as usize
                } else {
                    2 * (a0 == GOLDEN_FRAME) as usize + 2 * (l0 == GOLDEN_FRAME) as usize
                }
            }
        }
        (Some(m), None) | (None, Some(m)) => {
            if !m.is_inter_block() || (m.ref_frame[0] == LAST_FRAME && !m.has_second_ref()) {
                2
            } else if !m.has_second_ref() {
                4 * (m.ref_frame[0] == GOLDEN_FRAME) as usize
            } else {
                3 * (m.ref_frame[0] == GOLDEN_FRAME || m.ref_frame[1] == GOLDEN_FRAME) as usize
            }
        }
        (None, None) => 2,
    }
}

/// `vp9_get_reference_mode_context` — single-vs-compound prediction context.
fn reference_mode_context(a: Option<&ModeInfo>, l: Option<&ModeInfo>, _sb: [bool; 4], fixed: usize) -> usize {
    let f = fixed as i8;
    match (a, l) {
        (Some(a), Some(l)) => {
            if !a.has_second_ref() && !l.has_second_ref() {
                ((a.ref_frame[0] == f) ^ (l.ref_frame[0] == f)) as usize
            } else if !a.has_second_ref() {
                2 + (a.ref_frame[0] == f || !a.is_inter_block()) as usize
            } else if !l.has_second_ref() {
                2 + (l.ref_frame[0] == f || !l.is_inter_block()) as usize
            } else {
                4
            }
        }
        (Some(m), None) | (None, Some(m)) => {
            if !m.has_second_ref() {
                (m.ref_frame[0] == f) as usize
            } else {
                3
            }
        }
        (None, None) => 1,
    }
}

/// `vp9_get_pred_context_comp_ref_p` — the compound-reference bit context.
fn comp_ref_context(a: Option<&ModeInfo>, l: Option<&ModeInfo>, sb: [bool; 4], fc: &FrameContext) -> usize {
    let fixed = fc.comp_fixed_ref as i8;
    let var0 = fc.comp_var_ref[0] as i8;
    let var1 = fc.comp_var_ref[1] as i8;
    let var_ref_idx = 1 - sb[fc.comp_fixed_ref] as usize;
    match (a, l) {
        (Some(a), Some(l)) => {
            let ai = !a.is_inter_block();
            let li = !l.is_inter_block();
            if ai && li {
                2
            } else if ai || li {
                let e = if ai { l } else { a };
                if !e.has_second_ref() {
                    1 + 2 * (e.ref_frame[0] != var1) as usize
                } else {
                    1 + 2 * (e.ref_frame[var_ref_idx] != var1) as usize
                }
            } else {
                let a_sg = !a.has_second_ref();
                let l_sg = !l.has_second_ref();
                let vrfa = if a_sg { a.ref_frame[0] } else { a.ref_frame[var_ref_idx] };
                let vrfl = if l_sg { l.ref_frame[0] } else { l.ref_frame[var_ref_idx] };
                if vrfa == vrfl && var1 == vrfa {
                    0
                } else if l_sg && a_sg {
                    if (vrfa == fixed && vrfl == var0) || (vrfl == fixed && vrfa == var0) {
                        4
                    } else if vrfa == vrfl {
                        3
                    } else {
                        1
                    }
                } else if l_sg || a_sg {
                    let vrfc = if l_sg { vrfa } else { vrfl };
                    let rfs = if a_sg { vrfa } else { vrfl };
                    if vrfc == var1 && rfs != var1 {
                        1
                    } else if rfs == var1 && vrfc != var1 {
                        2
                    } else {
                        4
                    }
                } else if vrfa == vrfl {
                    4
                } else {
                    2
                }
            }
        }
        (Some(e), None) | (None, Some(e)) => {
            if !e.is_inter_block() {
                2
            } else if e.has_second_ref() {
                4 * (e.ref_frame[var_ref_idx] != var1) as usize
            } else {
                3 * (e.ref_frame[0] != var1) as usize
            }
        }
        (None, None) => 2,
    }
}

/// `vpx_read_bit` — a literal bit (probability 128).
#[inline]
fn read_bit(b: &mut BoolDecoder) -> u32 {
    b.read_bool(128)
}

/// `decode_term_subexp` (libvpx) — the sub-exponential magnitude for a prob update.
fn decode_term_subexp(b: &mut BoolDecoder) -> u32 {
    if read_bit(b) == 0 {
        return b.literal(4);
    }
    if read_bit(b) == 0 {
        return b.literal(4) + 16;
    }
    if read_bit(b) == 0 {
        return b.literal(5) + 32;
    }
    // decode_uniform: l=8, m=65.
    let v = b.literal(7);
    if v < 65 {
        return v + 64;
    }
    ((v << 1) - 65 + read_bit(b)) + 64
}

/// `vp9_diff_update_prob` — conditionally update a probability in place.
fn diff_update(b: &mut BoolDecoder, p: &mut u8) {
    if b.read_bool(252) == 1 {
        let delta = decode_term_subexp(b) as i32;
        *p = inv_remap_prob(delta, *p as i32);
    }
}

fn read_tx_mode(b: &mut BoolDecoder, lossless: bool) -> usize {
    if lossless {
        return 0;
    }
    let mut t = b.literal(2) as usize;
    if t == 3 {
        t += read_bit(b) as usize;
    }
    t
}

fn read_tx_mode_probs(b: &mut BoolDecoder, fc: &mut FrameContext) {
    for i in 0..2 {
        for j in 0..1 {
            diff_update(b, &mut fc.tx_p8x8[i][j]);
        }
    }
    for i in 0..2 {
        for j in 0..2 {
            diff_update(b, &mut fc.tx_p16x16[i][j]);
        }
    }
    for i in 0..2 {
        for j in 0..3 {
            diff_update(b, &mut fc.tx_p32x32[i][j]);
        }
    }
}

fn read_coef_probs(b: &mut BoolDecoder, fc: &mut FrameContext) {
    let max_tx = TX_MODE_TO_BIGGEST_TX[fc.tx_mode] as usize;
    for tx in 0..=max_tx {
        if read_bit(b) == 1 {
            for i in 0..2 {
                for j in 0..2 {
                    for k in 0..6 {
                        let nctx = if k == 0 { 3 } else { 6 };
                        for l in 0..nctx {
                            for m in 0..3 {
                                diff_update(b, &mut fc.coef_probs[tx][i][j][k][l][m]);
                            }
                        }
                    }
                }
            }
        }
    }
}

/// Parse the compressed header into a [`FrameContext`] (key/intra or inter).
fn parse_compressed_header(data: &[u8], h: &FrameHeader, pre_fc: &FrameContext) -> crate::Result<FrameContext> {
    let mut b = BoolDecoder::new(data)?;
    // Forward updates are applied on top of the loaded (saved) frame context.
    let mut fc = pre_fc.clone();
    fc.tx_mode = read_tx_mode(&mut b, h.lossless);
    if fc.tx_mode == TX_MODE_SELECT {
        read_tx_mode_probs(&mut b, &mut fc);
    }
    read_coef_probs(&mut b, &mut fc);
    for k in 0..3 {
        diff_update(&mut b, &mut fc.skip_probs[k]);
    }

    let intra_only = h.key_frame || h.intra_only;
    if !intra_only {
        // Inter-mode probs.
        for i in 0..7 {
            diff_update_slice(&mut b, &mut fc.inter_mode_probs[i]);
        }
        if h.interp_filter == 4 {
            // SWITCHABLE
            for j in 0..4 {
                diff_update_slice(&mut b, &mut fc.switchable_interp_prob[j]);
            }
        }
        diff_update_slice(&mut b, &mut fc.intra_inter_prob);

        // Reference mode (single / compound / select).
        let compound_allowed =
            h.ref_sign_bias[1] != h.ref_sign_bias[0] || h.ref_sign_bias[2] != h.ref_sign_bias[0];
        fc.reference_mode = if compound_allowed {
            if read_bit(&mut b) == 1 {
                if read_bit(&mut b) == 1 {
                    2
                } else {
                    1
                }
            } else {
                0
            }
        } else {
            0
        };
        if fc.reference_mode != 0 {
            setup_compound_reference_mode(&mut fc, &h.ref_sign_bias);
        }
        // Reference-mode probs.
        if fc.reference_mode == 2 {
            diff_update_slice(&mut b, &mut fc.comp_inter_prob);
        }
        if fc.reference_mode != 1 {
            for i in 0..5 {
                diff_update(&mut b, &mut fc.single_ref_prob[i][0]);
                diff_update(&mut b, &mut fc.single_ref_prob[i][1]);
            }
        }
        if fc.reference_mode != 0 {
            diff_update_slice(&mut b, &mut fc.comp_ref_prob);
        }

        // Y mode + partition probs.
        for j in 0..4 {
            diff_update_slice(&mut b, &mut fc.y_mode_prob[j]);
        }
        for j in 0..16 {
            diff_update_slice(&mut b, &mut fc.partition_prob[j]);
        }

        // MV probs.
        read_mv_probs(&mut b, &mut fc.nmvc, h.allow_high_precision_mv);
    }
    Ok(fc)
}

/// `vp9_setup_compound_reference_mode` — pick the fixed/var compound refs from
/// the sign biases (LAST=1, GOLDEN=2, ALTREF=3).
fn setup_compound_reference_mode(fc: &mut FrameContext, sign_bias: &[bool; 3]) {
    if sign_bias[0] == sign_bias[1] {
        fc.comp_fixed_ref = 3;
        fc.comp_var_ref = [1, 2];
    } else if sign_bias[0] == sign_bias[2] {
        fc.comp_fixed_ref = 2;
        fc.comp_var_ref = [1, 3];
    } else {
        fc.comp_fixed_ref = 1;
        fc.comp_var_ref = [2, 3];
    }
}

/// `read_mv_probs` — update the MV entropy model.
fn read_mv_probs(b: &mut BoolDecoder, nmvc: &mut NmvContext, allow_hp: bool) {
    update_mv_probs(b, &mut nmvc.joints);
    for c in &mut nmvc.comps {
        update_mv_probs(b, std::slice::from_mut(&mut c.sign));
        update_mv_probs(b, &mut c.classes);
        update_mv_probs(b, &mut c.class0);
        update_mv_probs(b, &mut c.bits);
    }
    for c in &mut nmvc.comps {
        for j in 0..2 {
            update_mv_probs(b, &mut c.class0_fp[j]);
        }
        update_mv_probs(b, &mut c.fp);
    }
    if allow_hp {
        for c in &mut nmvc.comps {
            update_mv_probs(b, std::slice::from_mut(&mut c.class0_hp));
            update_mv_probs(b, std::slice::from_mut(&mut c.hp));
        }
    }
}

// ---- frame buffer -------------------------------------------------------

struct Plane {
    buf: Vec<u16>,
    stride: usize,
    width: usize,
    height: usize,
    ss_x: usize,
    ss_y: usize,
}

impl Plane {
    fn new(width: usize, height: usize, ss_x: usize, ss_y: usize) -> Plane {
        let w = (width + ss_x) >> ss_x;
        let h = (height + ss_y) >> ss_y;
        // Pad the stride/height to a superblock so edge reads stay in-bounds.
        let stride = (w + 64 + 8).next_power_of_two();
        Plane { buf: vec![0u16; stride * (h + 64 + 8)], stride, width: w, height: h, ss_x, ss_y }
    }
}

/// `get_tile_offset` — the mi-column/row start of tile `idx` (libvpx
/// `vp9_tile_common.c`), aligned to a superblock and clamped to the frame.
fn tile_offset(idx: usize, mis: usize, log2: u32) -> usize {
    let sb_cols = (mis + 7) >> 3;
    (((idx * sb_cols) >> log2) << 3).min(mis)
}

// ---- segmentation (ISO/VP9 §6.4.10) -------------------------------------

/// `vp9_segment_tree` — the 8-segment id tree (leaves are `-segment`).
const SEGMENT_TREE: [i8; 14] = [2, 4, 6, 8, 10, 12, 0, -1, -2, -3, -4, -5, -6, -7];
const SEG_LVL_ALT_Q: usize = 0;
const SEG_LVL_REF_FRAME: usize = 2;
const SEG_LVL_SKIP: usize = 3;

/// Per-frame segmentation parameters (copied from the header).
#[derive(Clone, Default)]
struct Seg {
    enabled: bool,
    update_map: bool,
    temporal_update: bool,
    abs_delta: bool,
    tree_probs: [u8; 7],
    pred_probs: [u8; 3],
    feature_enabled: [[bool; 4]; 8],
    feature_data: [[i32; 4]; 8],
}

impl Seg {
    fn from_header(h: &FrameHeader) -> Seg {
        Seg {
            enabled: h.seg_enabled,
            update_map: h.seg_update_map,
            temporal_update: h.seg_temporal_update,
            abs_delta: h.seg_abs_delta,
            tree_probs: h.seg_tree_probs,
            pred_probs: h.seg_pred_probs,
            feature_enabled: h.seg_feature_enabled,
            feature_data: h.seg_feature_data,
        }
    }
    fn active(&self, sid: usize, feature: usize) -> bool {
        self.enabled && self.feature_enabled[sid][feature]
    }
    /// `vp9_get_qindex` — the per-segment quantizer index.
    fn qindex(&self, sid: usize, base_q: i32) -> i32 {
        if self.active(sid, SEG_LVL_ALT_Q) {
            let data = self.feature_data[sid][SEG_LVL_ALT_Q];
            let q = if self.abs_delta { data } else { base_q + data };
            q.clamp(0, 255)
        } else {
            base_q
        }
    }
}

// ---- the decoder --------------------------------------------------------

pub struct Reconstructor {
    fc: FrameContext,
    lossless: bool,
    dq_y: [(i32, i32); 8],
    dq_uv: [(i32, i32); 8],
    seg: Seg,
    prev_seg_map: Option<std::sync::Arc<Vec<u8>>>,
    cur_seg_map: Vec<u8>,
    /// mi bounds of the tile currently being decoded (for neighbour clipping).
    tile_col_start: usize,
    tile_col_end: usize,
    tile_row_start: usize,
    mi_rows: usize,
    mi_cols: usize,
    mi: Vec<ModeInfo>,
    planes: [Plane; 3],
    above_seg: Vec<u8>,
    left_seg: [u8; 8],
    above_ctx: [Vec<u8>; 3],
    left_ctx: [[u8; 16]; 3],
    // ---- inter-frame state ----
    is_inter_frame: bool,
    interp_filter: u32,
    allow_hp: bool,
    /// `ref_frame_sign_bias` indexed by reference (INTRA..ALTREF).
    sign_bias: [bool; 4],
    /// The three active references (LAST/GOLDEN/ALTREF), resolved from the map.
    refs: [Option<std::sync::Arc<RefFrame>>; 3],
    counts: FrameCounts,
    /// Previous frame's per-mi motion records (for temporal MV prediction).
    prev_mvs: Option<std::sync::Arc<Vec<MvRef>>>,
    use_prev_mvs: bool,
    /// Maximum pixel value `(1<<bit_depth)-1` (255 at 8-bit), for clamping.
    max_px: i32,
    /// Per-transform-block scratch, reused across blocks to avoid re-allocating
    /// / re-zeroing 4 KB + 1 KB on every coefficient decode. `decode_coefs`
    /// clears `dqcoeff[..n²]` and `token_cache[..n²]` itself, so stale tail data
    /// past the current transform size is never read.
    dqcoeff: Vec<i32>,
    token_cache: Vec<u8>,
}

impl Reconstructor {
    /// The previous frame's MV record at this mi position, if temporal MV
    /// prediction is enabled for this frame.
    fn prev_mv(&self, mi_row: usize, mi_col: usize) -> Option<&MvRef> {
        if self.use_prev_mvs {
            self.prev_mvs.as_ref().map(|g| &g[mi_row * self.mi_cols + mi_col])
        } else {
            None
        }
    }

    /// The mi cells a block covers, clipped to the frame.
    fn seg_mis(&self, mi_row: usize, mi_col: usize, bsize: usize) -> (usize, usize) {
        let bw8 = (1usize << B_WIDTH_LOG2[bsize] >> 1).max(1);
        let bh8 = (1usize << B_HEIGHT_LOG2[bsize] >> 1).max(1);
        (bw8.min(self.mi_cols - mi_col), bh8.min(self.mi_rows - mi_row))
    }
    fn set_seg_id(&mut self, mi_row: usize, mi_col: usize, x_mis: usize, y_mis: usize, sid: u8) {
        for y in 0..y_mis {
            for x in 0..x_mis {
                self.cur_seg_map[(mi_row + y) * self.mi_cols + mi_col + x] = sid;
            }
        }
    }
    fn copy_seg_id(&mut self, mi_row: usize, mi_col: usize, x_mis: usize, y_mis: usize) {
        if let Some(prev) = self.prev_seg_map.clone() {
            for y in 0..y_mis {
                for x in 0..x_mis {
                    let off = (mi_row + y) * self.mi_cols + mi_col + x;
                    self.cur_seg_map[off] = prev[off];
                }
            }
        }
    }
    /// `dec_get_segment_id` — the minimum predicted segment over covered cells.
    fn prev_seg_min(&self, mi_row: usize, mi_col: usize, x_mis: usize, y_mis: usize) -> u8 {
        match &self.prev_seg_map {
            Some(prev) => {
                let mut m = 7u8;
                for y in 0..y_mis {
                    for x in 0..x_mis {
                        m = m.min(prev[(mi_row + y) * self.mi_cols + mi_col + x]);
                    }
                }
                m
            }
            None => 0,
        }
    }
    fn read_segment_id_tree(&mut self, b: &mut BoolDecoder) -> u8 {
        crate::token::read_tree(b, &SEGMENT_TREE, &self.seg.tree_probs) as u8
    }
    fn read_intra_segment_id(&mut self, b: &mut BoolDecoder, mi_row: usize, mi_col: usize, bsize: usize) -> u8 {
        if !self.seg.enabled {
            return 0;
        }
        let (xm, ym) = self.seg_mis(mi_row, mi_col, bsize);
        if !self.seg.update_map {
            self.copy_seg_id(mi_row, mi_col, xm, ym);
            return 0;
        }
        let sid = self.read_segment_id_tree(b);
        self.set_seg_id(mi_row, mi_col, xm, ym, sid);
        sid
    }
    fn read_inter_segment_id(&mut self, b: &mut BoolDecoder, mi_row: usize, mi_col: usize, bsize: usize, above: Option<&ModeInfo>, left: Option<&ModeInfo>) -> (u8, bool) {
        if !self.seg.enabled {
            return (0, false);
        }
        let (xm, ym) = self.seg_mis(mi_row, mi_col, bsize);
        let predicted = self.prev_seg_min(mi_row, mi_col, xm, ym);
        if !self.seg.update_map {
            self.copy_seg_id(mi_row, mi_col, xm, ym);
            return (predicted, false);
        }
        let mut seg_pred = false;
        let sid = if self.seg.temporal_update {
            let ctx = above.map_or(0, |m| m.seg_id_predicted as usize) + left.map_or(0, |m| m.seg_id_predicted as usize);
            seg_pred = b.read_bool(self.seg.pred_probs[ctx]) != 0;
            if seg_pred {
                predicted
            } else {
                self.read_segment_id_tree(b)
            }
        } else {
            self.read_segment_id_tree(b)
        };
        self.set_seg_id(mi_row, mi_col, xm, ym, sid);
        (sid, seg_pred)
    }
}

/// A fully decoded frame, kept (padded, frame at origin) so later frames can use
/// it as a motion-compensation reference. Motion compensation clamps tap
/// coordinates to `[0, w)`×`[0, h)`, which reproduces libvpx's edge extension
/// bit-for-bit without a separate border buffer.
#[derive(Clone)]
pub struct RefFrame {
    pub planes: [Vec<u16>; 3],
    pub stride: [usize; 3],
    /// Per-plane visible width/height in pixels.
    pub w: [usize; 3],
    pub h: [usize; 3],
    // Used by inter motion compensation (chroma MV scaling).
    #[allow(dead_code)]
    pub ss_x: usize,
    #[allow(dead_code)]
    pub ss_y: usize,
    /// Bit depth (8/10/12) — drives the output sample width.
    pub bit_depth: u32,
}

/// Decode a key/intra frame to planar pixels: returns (Y, U, V) planes with
/// their per-row widths, plus (width, height). Thin wrapper over [`decode_frame`]
/// retained for the single-frame dump test and external callers.
#[allow(dead_code)]
pub fn decode_intra_frame(
    h: &FrameHeader,
    data: &[u8],
) -> crate::Result<([Vec<u8>; 3], [usize; 3], [usize; 3])> {
    let no_refs: [Option<std::sync::Arc<RefFrame>>; 3] = [None, None, None];
    let (rf, _fc, _mvs, _seg) = decode_frame(h, data, &no_refs, &FrameContext::defaults(), false, None, false, None)?;
    let mut out: [Vec<u8>; 3] = [Vec::new(), Vec::new(), Vec::new()];
    let mut widths = [0usize; 3];
    let mut heights = [0usize; 3];
    for p in 0..3 {
        let (stride, w, hh) = (rf.stride[p], rf.w[p], rf.h[p]);
        let mut v = Vec::with_capacity(w * hh);
        for y in 0..hh {
            // 8-bit dump helper: planes are stored u16, downcast to bytes.
            v.extend(rf.planes[p][y * stride..y * stride + w].iter().map(|&px| px as u8));
        }
        out[p] = v;
        widths[p] = w;
        heights[p] = hh;
    }
    Ok((out, widths, heights))
}

/// Decode one frame (key/intra/inter) to a [`RefFrame`] plus the frame's final
/// entropy context (forward-updated, then backward-adapted) for the decoder to
/// save. `pre_fc` is the loaded saved context; `refs` the three active refs.
#[allow(clippy::too_many_arguments)]
pub fn decode_frame(
    h: &FrameHeader,
    data: &[u8],
    refs: &[Option<std::sync::Arc<RefFrame>>; 3],
    pre_fc: &FrameContext,
    last_frame_key: bool,
    prev_mvs: Option<std::sync::Arc<Vec<MvRef>>>,
    use_prev_mvs: bool,
    prev_seg_map: Option<std::sync::Arc<Vec<u8>>>,
) -> crate::Result<(RefFrame, FrameContext, std::sync::Arc<Vec<MvRef>>, std::sync::Arc<Vec<u8>>)> {
    let start = h.uncompressed_bytes;
    let end = start.saturating_add(h.header_size as usize);
    if h.header_size == 0 || end > data.len() {
        return Err(crate::Error::invalid("vp9: compressed header out of bounds"));
    }
    let fc = parse_compressed_header(&data[start..end], h, pre_fc)?;
    let _ = refs;

    let seg = Seg::from_header(h);
    // Per-segment dequantizers (the segment's quantizer index drives dc/ac).
    let mut dq_y = [(0i32, 0i32); 8];
    let mut dq_uv = [(0i32, 0i32); 8];
    for s in 0..8 {
        let qidx = seg.qindex(s, h.base_q_idx as i32);
        let dq = Dequant::new(qidx, h.delta_q_y_dc, h.delta_q_uv_dc, h.delta_q_uv_ac, h.bit_depth.max(8));
        dq_y[s] = (dq.y_dc, dq.y_ac);
        dq_uv[s] = (dq.uv_dc, dq.uv_ac);
    }
    let (ss_x, ss_y) = (h.subsampling_x as usize, h.subsampling_y as usize);
    let (w, hgt) = (h.width as usize, h.height as usize);
    let mi_cols = (w + 7) / MI_SIZE;
    let mi_rows = (hgt + 7) / MI_SIZE;
    let aligned_cols = (mi_cols + 7) & !7;

    let mut rec = Reconstructor {
        fc,
        lossless: h.lossless,
        dq_y,
        dq_uv,
        seg,
        prev_seg_map,
        cur_seg_map: vec![0u8; mi_rows * mi_cols],
        tile_col_start: 0,
        tile_col_end: mi_cols,
        tile_row_start: 0,
        mi_rows,
        mi_cols,
        mi: vec![ModeInfo::default(); mi_rows * mi_cols],
        planes: [
            Plane::new(w, hgt, 0, 0),
            Plane::new(w, hgt, ss_x, ss_y),
            Plane::new(w, hgt, ss_x, ss_y),
        ],
        above_seg: vec![0u8; aligned_cols],
        left_seg: [0u8; 8],
        above_ctx: [
            vec![0u8; 2 * aligned_cols],
            vec![0u8; 2 * aligned_cols],
            vec![0u8; 2 * aligned_cols],
        ],
        left_ctx: [[0u8; 16]; 3],
        is_inter_frame: !(h.key_frame || h.intra_only),
        interp_filter: h.interp_filter,
        allow_hp: h.allow_high_precision_mv,
        sign_bias: [false, h.ref_sign_bias[0], h.ref_sign_bias[1], h.ref_sign_bias[2]],
        refs: [refs[0].clone(), refs[1].clone(), refs[2].clone()],
        counts: FrameCounts::zeroed(),
        prev_mvs,
        use_prev_mvs,
        max_px: (1i32 << h.bit_depth.max(8)) - 1,
        dqcoeff: vec![0i32; 1024],
        token_cache: vec![0u8; 1024],
    };

    rec.decode_tiles(&data[end..], h.tile_cols_log2, h.tile_rows_log2)?;

    // In-loop deblocking filter over the fully reconstructed frame.
    {
        let mut lf_planes: Vec<(&mut [u16], usize, usize, usize)> = rec
            .planes
            .iter_mut()
            .map(|pl| (pl.buf.as_mut_slice(), pl.stride, pl.ss_x, pl.ss_y))
            .collect();
        crate::loopfilter::loop_filter_frame(&mut lf_planes, &rec.mi, rec.mi_rows, rec.mi_cols, h);
    }

    // Backward probability adaptation: nudge the working context toward the
    // empirical symbol counts (only when this frame refreshes a context and is
    // not frame-parallel). `pre_fc` is the merge prior, not the forward-updated
    // working context.
    let mut out_fc = rec.fc;
    if h.refresh_frame_context && !h.frame_parallel_decoding_mode {
        let intra_only = h.key_frame || h.intra_only;
        // Coefficient adaptation update factor depends on frame position.
        let update_factor = if intra_only || !last_frame_key { 112 } else { 128 };
        let count_sat = 24;
        let tx_select = out_fc.tx_mode == TX_MODE_SELECT;
        adapt_coef_probs(&mut out_fc, pre_fc, &rec.counts, count_sat, update_factor);
        if !intra_only {
            adapt_mode_probs(&mut out_fc, pre_fc, &rec.counts, h.interp_filter == 4, tx_select);
            adapt_mv_probs(&mut out_fc, pre_fc, &rec.counts, h.allow_high_precision_mv);
        }
    }

    // Hand back the padded planes as a reference frame (frame at origin).
    let planes = [
        std::mem::take(&mut rec.planes[0].buf),
        std::mem::take(&mut rec.planes[1].buf),
        std::mem::take(&mut rec.planes[2].buf),
    ];
    let rf = RefFrame {
        planes,
        stride: [rec.planes[0].stride, rec.planes[1].stride, rec.planes[2].stride],
        w: [rec.planes[0].width, rec.planes[1].width, rec.planes[2].width],
        h: [rec.planes[0].height, rec.planes[1].height, rec.planes[2].height],
        ss_x,
        ss_y,
        bit_depth: h.bit_depth.max(8),
    };
    // Per-mi motion records for the next frame's temporal MV prediction.
    let mvs: Vec<MvRef> = rec
        .mi
        .iter()
        .map(|m| MvRef { ref_frame: m.ref_frame, mv: m.mv })
        .collect();
    Ok((rf, out_fc, std::sync::Arc::new(mvs), std::sync::Arc::new(rec.cur_seg_map)))
}

impl Reconstructor {
    fn decode_tiles(&mut self, data: &[u8], tile_cols_log2: u32, tile_rows_log2: u32) -> crate::Result<()> {
        let tile_cols = 1usize << tile_cols_log2;
        let tile_rows = 1usize << tile_rows_log2;
        let mut off = 0usize;
        for tr in 0..tile_rows {
            let mr_start = tile_offset(tr, self.mi_rows, tile_rows_log2);
            let mr_end = tile_offset(tr + 1, self.mi_rows, tile_rows_log2);
            self.tile_row_start = mr_start;
            // The above contexts are independent per tile row.
            for c in self.above_ctx.iter_mut() {
                c.iter_mut().for_each(|v| *v = 0);
            }
            self.above_seg.iter_mut().for_each(|v| *v = 0);
            for tc in 0..tile_cols {
                self.tile_col_start = tile_offset(tc, self.mi_cols, tile_cols_log2);
                self.tile_col_end = tile_offset(tc + 1, self.mi_cols, tile_cols_log2);
                let last = tr == tile_rows - 1 && tc == tile_cols - 1;
                let tile_data = if last {
                    &data[off..]
                } else {
                    if off + 4 > data.len() {
                        return Err(crate::Error::invalid("vp9: tile size overruns frame"));
                    }
                    let sz = u32::from_be_bytes([data[off], data[off + 1], data[off + 2], data[off + 3]]) as usize;
                    off += 4;
                    if off + sz > data.len() {
                        return Err(crate::Error::invalid("vp9: tile data overruns frame"));
                    }
                    let d = &data[off..off + sz];
                    off += sz;
                    d
                };
                let mut b = BoolDecoder::new(tile_data)?;
                for mi_row in (mr_start..mr_end).step_by(8) {
                    // Clear left contexts at the start of each superblock row.
                    self.left_seg = [0; 8];
                    self.left_ctx = [[0; 16]; 3];
                    for mi_col in (self.tile_col_start..self.tile_col_end).step_by(8) {
                        self.decode_partition(&mut b, mi_row, mi_col, 12, 4)?;
                    }
                }
            }
        }
        Ok(())
    }

    /// `decode_partition` — recursive split of a 64×64 superblock.
    /// `bsize` is the current BLOCK_SIZE, `n4x4_l2` = log2 of its 4×4 width.
    fn decode_partition(
        &mut self,
        b: &mut BoolDecoder,
        mi_row: usize,
        mi_col: usize,
        bsize: usize,
        n4x4_l2: usize,
    ) -> crate::Result<()> {
        if mi_row >= self.mi_rows || mi_col >= self.mi_cols {
            return Ok(());
        }
        let n8x8_l2 = n4x4_l2 - 1;
        let num_8x8 = 1 << n8x8_l2;
        let hbs = num_8x8 >> 1;
        let has_rows = mi_row + hbs < self.mi_rows;
        let has_cols = mi_col + hbs < self.mi_cols;

        let ctx = partition_plane_context(&self.above_seg, &self.left_seg, mi_row, mi_col, n8x8_l2);
        // Key/intra frames use the fixed kf partition probs; inter frames use the
        // frame-context (adapted + updated) partition probs.
        let probs = if self.is_inter_frame {
            &self.fc.partition_prob[ctx]
        } else {
            &KF_PARTITION_PROBS[ctx]
        };
        let partition = read_partition(b, probs, has_rows, has_cols);
        // Partition symbols are adapted only on inter frames (kf uses fixed probs).
        if self.is_inter_frame {
            self.counts.partition[ctx][partition] += 1;
        }
        let subsize = subsize(partition, bsize) as usize;

        if hbs == 0 {
            // 8×8 split into 4×4 sub-blocks: decode one sub-8×8 block.
            self.decode_block(b, mi_row, mi_col, subsize, n4x4_l2, n4x4_l2)?;
        } else {
            match partition {
                PARTITION_NONE => self.decode_block(b, mi_row, mi_col, subsize, n4x4_l2, n4x4_l2)?,
                PARTITION_HORZ => {
                    self.decode_block(b, mi_row, mi_col, subsize, n4x4_l2, n8x8_l2)?;
                    if has_rows {
                        self.decode_block(b, mi_row + hbs, mi_col, subsize, n4x4_l2, n8x8_l2)?;
                    }
                }
                PARTITION_VERT => {
                    self.decode_block(b, mi_row, mi_col, subsize, n8x8_l2, n4x4_l2)?;
                    if has_cols {
                        self.decode_block(b, mi_row, mi_col + hbs, subsize, n8x8_l2, n4x4_l2)?;
                    }
                }
                PARTITION_SPLIT => {
                    self.decode_partition(b, mi_row, mi_col, subsize, n8x8_l2)?;
                    self.decode_partition(b, mi_row, mi_col + hbs, subsize, n8x8_l2)?;
                    self.decode_partition(b, mi_row + hbs, mi_col, subsize, n8x8_l2)?;
                    self.decode_partition(b, mi_row + hbs, mi_col + hbs, subsize, n8x8_l2)?;
                }
                _ => unreachable!(),
            }
        }

        // Update the partition context at the leaf nodes.
        if bsize >= BLOCK_8X8 && (bsize == BLOCK_8X8 || partition != PARTITION_SPLIT) {
            update_partition_context(
                &mut self.above_seg,
                &mut self.left_seg,
                mi_row,
                mi_col,
                subsize,
                num_8x8,
            );
        }
        Ok(())
    }

    fn above_mi(&self, mi_row: usize, mi_col: usize) -> Option<ModeInfo> {
        // Neighbours are unavailable across a tile boundary (tiles are independent).
        (mi_row > self.tile_row_start).then(|| self.mi[(mi_row - 1) * self.mi_cols + mi_col])
    }
    fn left_mi(&self, mi_row: usize, mi_col: usize) -> Option<ModeInfo> {
        (mi_col > self.tile_col_start).then(|| self.mi[mi_row * self.mi_cols + mi_col - 1])
    }

    fn decode_block(
        &mut self,
        b: &mut BoolDecoder,
        mi_row: usize,
        mi_col: usize,
        bsize: usize,
        bwl: usize,
        bhl: usize,
    ) -> crate::Result<()> {
        let bw = 1usize << (bwl - 1); // mi units wide
        let bh = 1usize << (bhl - 1);
        let mi = self.read_mode_info(b, mi_row, mi_col, bsize);

        // Store the mode info across all covered mi cells (for neighbour lookup).
        let x_mis = bw.min(self.mi_cols - mi_col);
        let y_mis = bh.min(self.mi_rows - mi_row);
        for y in 0..y_mis {
            for x in 0..x_mis {
                self.mi[(mi_row + y) * self.mi_cols + mi_col + x] = mi;
            }
        }

        for plane in 0..3 {
            self.reconstruct_plane(b, &mi, plane, mi_row, mi_col, bsize, bwl, bhl)?;
        }
        Ok(())
    }

    fn read_mode_info(&mut self, b: &mut BoolDecoder, mi_row: usize, mi_col: usize, bsize: usize) -> ModeInfo {
        if self.is_inter_frame {
            return self.read_inter_frame_mode_info(b, mi_row, mi_col, bsize);
        }
        let above = self.above_mi(mi_row, mi_col);
        let left = self.left_mi(mi_row, mi_col);
        let mut mi = ModeInfo { sb_type: bsize as u8, is_inter: false, ..Default::default() };
        mi.segment_id = self.read_intra_segment_id(b, mi_row, mi_col, bsize);

        // skip flag.
        let sctx = skip_context(above.as_ref(), left.as_ref());
        mi.skip = b.read_bool(self.fc.skip_probs[sctx]) != 0;

        // tx size.
        mi.tx_size = self.read_tx_size(b, bsize, &mi, above.as_ref(), left.as_ref(), true);

        // Y mode(s).
        let ar = above.as_ref();
        let lr = left.as_ref();
        match bsize {
            0 => {
                for i in 0..4 {
                    let p = *kf_y_mode_probs(&mi, ar, lr, i);
                    mi.bmi[i] = read_intra_mode(b, &p);
                }
                mi.mode = mi.bmi[3];
            }
            1 => {
                let p0 = *kf_y_mode_probs(&mi, ar, lr, 0);
                let m0 = read_intra_mode(b, &p0);
                mi.bmi[0] = m0;
                mi.bmi[2] = m0;
                let p1 = *kf_y_mode_probs(&mi, ar, lr, 1);
                let m1 = read_intra_mode(b, &p1);
                mi.bmi[1] = m1;
                mi.bmi[3] = m1;
                mi.mode = m1;
            }
            2 => {
                let p0 = *kf_y_mode_probs(&mi, ar, lr, 0);
                let m0 = read_intra_mode(b, &p0);
                mi.bmi[0] = m0;
                mi.bmi[1] = m0;
                let p2 = *kf_y_mode_probs(&mi, ar, lr, 2);
                let m2 = read_intra_mode(b, &p2);
                mi.bmi[2] = m2;
                mi.bmi[3] = m2;
                mi.mode = m2;
            }
            _ => {
                let p = *kf_y_mode_probs(&mi, ar, lr, 0);
                mi.mode = read_intra_mode(b, &p);
            }
        }
        mi.uv_mode = read_intra_mode(b, kf_uv_mode_probs(mi.mode));
        mi
    }

    fn read_tx_size(&mut self, b: &mut BoolDecoder, bsize: usize, cur: &ModeInfo, above: Option<&ModeInfo>, left: Option<&ModeInfo>, allow_select: bool) -> u8 {
        let max_tx = MAX_TXSIZE[bsize] as usize;
        if allow_select && self.fc.tx_mode == TX_MODE_SELECT && bsize >= BLOCK_8X8 {
            let ctx = tx_size_context(cur, above, left);
            match max_tx {
                1 => {
                    let t = read_selected_tx_size(b, &self.fc.tx_p8x8[ctx], max_tx);
                    self.counts.tx_p8x8[ctx][t as usize] += 1;
                    t
                }
                2 => {
                    let t = read_selected_tx_size(b, &self.fc.tx_p16x16[ctx], max_tx);
                    self.counts.tx_p16x16[ctx][t as usize] += 1;
                    t
                }
                _ => {
                    let t = read_selected_tx_size(b, &self.fc.tx_p32x32[ctx], max_tx);
                    self.counts.tx_p32x32[ctx][t as usize] += 1;
                    t
                }
            }
        } else {
            max_tx.min(TX_MODE_TO_BIGGEST_TX[self.fc.tx_mode] as usize) as u8
        }
    }

    // ---- Component 6: inter mode-info decode -----------------------------

    fn read_inter_frame_mode_info(&mut self, b: &mut BoolDecoder, mi_row: usize, mi_col: usize, bsize: usize) -> ModeInfo {
        let above = self.above_mi(mi_row, mi_col);
        let left = self.left_mi(mi_row, mi_col);
        let mut mi = ModeInfo { sb_type: bsize as u8, ..Default::default() };

        // segment_id (spatial tree or temporal prediction).
        let (sid, seg_pred) = self.read_inter_segment_id(b, mi_row, mi_col, bsize, above.as_ref(), left.as_ref());
        mi.segment_id = sid;
        mi.seg_id_predicted = seg_pred;
        let sidx = sid as usize;
        // skip (SEG_LVL_SKIP forces it).
        let sctx = skip_context(above.as_ref(), left.as_ref());
        mi.skip = if self.seg.active(sidx, SEG_LVL_SKIP) {
            true
        } else {
            let s = b.read_bool(self.fc.skip_probs[sctx]) != 0;
            self.counts.skip[sctx][s as usize] += 1;
            s
        };
        // is_inter (SEG_LVL_REF_FRAME forces the reference, hence inter/intra).
        let ictx = intra_inter_context(above.as_ref(), left.as_ref());
        let inter_block = if self.seg.active(sidx, SEG_LVL_REF_FRAME) {
            self.seg.feature_data[sidx][SEG_LVL_REF_FRAME] != INTRA_FRAME as i32
        } else {
            let ib = b.read_bool(self.fc.intra_inter_prob[ictx]) != 0;
            self.counts.intra_inter[ictx][ib as usize] += 1;
            ib
        };
        // tx size (allow_select depends on skip & inter).
        let allow_select = !mi.skip || !inter_block;
        mi.tx_size = self.read_tx_size(b, bsize, &mi, above.as_ref(), left.as_ref(), allow_select);
        mi.is_inter = inter_block;

        if inter_block {
            self.read_inter_block_mode_info(b, &mut mi, mi_row, mi_col, bsize, above.as_ref(), left.as_ref());
        } else {
            self.read_intra_block_mode_info_inter(b, &mut mi, bsize);
            mi.ref_frame = [INTRA_FRAME, NONE_FRAME];
            mi.interp_filter = 3; // SWITCHABLE_FILTERS sentinel
        }
        mi
    }

    /// Intra block inside an inter frame (uses the frame's adapted y/uv probs).
    fn read_intra_block_mode_info_inter(&mut self, b: &mut BoolDecoder, mi: &mut ModeInfo, bsize: usize) {
        match bsize {
            0 => {
                for i in 0..4 {
                    mi.bmi[i] = self.read_y_mode(b, 0);
                }
                mi.mode = mi.bmi[3];
            }
            1 => {
                let m0 = self.read_y_mode(b, 0);
                mi.bmi[0] = m0;
                mi.bmi[2] = m0;
                let m1 = self.read_y_mode(b, 0);
                mi.bmi[1] = m1;
                mi.bmi[3] = m1;
                mi.mode = m1;
            }
            2 => {
                let m0 = self.read_y_mode(b, 0);
                mi.bmi[0] = m0;
                mi.bmi[1] = m0;
                let m2 = self.read_y_mode(b, 0);
                mi.bmi[2] = m2;
                mi.bmi[3] = m2;
                mi.mode = m2;
            }
            _ => {
                mi.mode = self.read_y_mode(b, SIZE_GROUP[bsize] as usize);
            }
        }
        mi.uv_mode = crate::block::read_intra_mode(b, &self.fc.uv_mode_prob[mi.mode as usize]);
        self.counts.uv_mode[mi.mode as usize][mi.uv_mode as usize] += 1;
    }

    fn read_y_mode(&mut self, b: &mut BoolDecoder, size_group: usize) -> u8 {
        let m = crate::block::read_intra_mode(b, &self.fc.y_mode_prob[size_group]);
        self.counts.y_mode[size_group][m as usize] += 1;
        m
    }

    fn read_ref_frames(&mut self, b: &mut BoolDecoder, mi: &mut ModeInfo, above: Option<&ModeInfo>, left: Option<&ModeInfo>) {
        // SEG_LVL_REF_FRAME forces a single reference with no coded bits.
        let sidx = mi.segment_id as usize;
        if self.seg.active(sidx, SEG_LVL_REF_FRAME) {
            mi.ref_frame[0] = self.seg.feature_data[sidx][SEG_LVL_REF_FRAME] as i8;
            mi.ref_frame[1] = NONE_FRAME;
            return;
        }
        // read_block_reference_mode: SELECT(2) reads comp_inter; else fixed.
        let mode = if self.fc.reference_mode == 2 {
            let ctx = reference_mode_context(above, left, self.sign_bias, self.fc.comp_fixed_ref);
            let m = b.read_bool(self.fc.comp_inter_prob[ctx]) as usize; // 0 single, 1 compound
            self.counts.comp_inter[ctx][m] += 1;
            m
        } else {
            self.fc.reference_mode
        };
        if mode == 1 {
            // COMPOUND_REFERENCE
            let idx = self.sign_bias[self.fc.comp_fixed_ref] as usize;
            let ctx = comp_ref_context(above, left, self.sign_bias, &self.fc);
            let bit = b.read_bool(self.fc.comp_ref_prob[ctx]) as usize;
            self.counts.comp_ref[ctx][bit] += 1;
            mi.ref_frame[idx] = self.fc.comp_fixed_ref as i8;
            mi.ref_frame[1 - idx] = self.fc.comp_var_ref[bit] as i8;
        } else {
            // SINGLE_REFERENCE
            let ctx0 = single_ref_p1(above, left);
            let bit0 = b.read_bool(self.fc.single_ref_prob[ctx0][0]) != 0;
            self.counts.single_ref[ctx0][0][bit0 as usize] += 1;
            if bit0 {
                let ctx1 = single_ref_p2(above, left);
                let bit1 = b.read_bool(self.fc.single_ref_prob[ctx1][1]) != 0;
                self.counts.single_ref[ctx1][1][bit1 as usize] += 1;
                mi.ref_frame[0] = if bit1 { ALTREF_FRAME } else { GOLDEN_FRAME };
            } else {
                mi.ref_frame[0] = LAST_FRAME;
            }
            mi.ref_frame[1] = NONE_FRAME;
        }
    }

    #[allow(clippy::too_many_arguments)]
    fn read_inter_block_mode_info(&mut self, b: &mut BoolDecoder, mi: &mut ModeInfo, mi_row: usize, mi_col: usize, bsize: usize, above: Option<&ModeInfo>, left: Option<&ModeInfo>) {
        let allow_hp = self.allow_hp;
        self.read_ref_frames(b, mi, above, left);
        let is_compound = mi.has_second_ref();
        let inter_mode_ctx = get_mode_context(&self.mi, self.mi_cols, self.mi_rows, self.tile_col_start, self.tile_col_end, mi_row, mi_col, bsize);

        // SEG_LVL_SKIP forces ZEROMV with no coded mode.
        let seg_skip = self.seg.active(mi.segment_id as usize, SEG_LVL_SKIP);
        if seg_skip {
            mi.mode = ZEROMV;
        } else if bsize >= BLOCK_8X8 {
            mi.mode = read_inter_mode(b, &self.fc.inter_mode_probs[inter_mode_ctx]);
            self.counts.inter_mode[inter_mode_ctx][(mi.mode - NEARESTMV) as usize] += 1;
        }
        mi.interp_filter = if self.interp_filter == 4 {
            self.read_switchable_interp_filter(b, above, left)
        } else {
            self.interp_filter as u8
        };

        let edges = self.mb_to_edges(mi_row, mi_col, bsize);
        let nrefs = 1 + is_compound as usize;

        if bsize < BLOCK_8X8 {
            let num_4x4_w = 1usize << B_WIDTH_LOG2[bsize];
            let num_4x4_h = 1usize << B_HEIGHT_LOG2[bsize];
            let mut best_ref_mvs = [(0i32, 0i32); 2];
            let mut got_new = false;
            let mut last_mode = ZEROMV;
            let mut idy = 0;
            while idy < 2 {
                let mut idx = 0;
                while idx < 2 {
                    let j = idy * 2 + idx;
                    let b_mode = read_inter_mode(b, &self.fc.inter_mode_probs[inter_mode_ctx]);
                    self.counts.inter_mode[inter_mode_ctx][(b_mode - NEARESTMV) as usize] += 1;
                    last_mode = b_mode;
                    let mut near_nearest = [(0i32, 0i32); 2];
                    if b_mode == NEARESTMV || b_mode == NEARMV {
                        for r in 0..nrefs {
                            near_nearest[r] = self.append_sub8x8_mvs(mi, b_mode, j, r, mi_row, mi_col, bsize, edges);
                        }
                    } else if b_mode == NEWMV && !got_new {
                        for r in 0..nrefs {
                            let frame = mi.ref_frame[r];
                            let (tmp, _) = find_mv_refs(&self.mi, self.mi_cols, self.mi_rows, self.tile_col_start, self.tile_col_end, mi_row, mi_col, bsize, frame, &self.sign_bias, NEWMV, -1, edges, self.prev_mv(mi_row, mi_col));
                            best_ref_mvs[r] = lower_mv_precision(tmp[0], allow_hp);
                        }
                        got_new = true;
                    }
                    let mut bmv = [(0i32, 0i32); 2];
                    self.assign_mv(b, b_mode, &mut bmv, &best_ref_mvs, &near_nearest, is_compound, allow_hp);
                    mi.bmi_mv[j] = bmv;
                    if num_4x4_h == 2 {
                        mi.bmi_mv[j + 2] = bmv;
                    }
                    if num_4x4_w == 2 {
                        mi.bmi_mv[j + 1] = bmv;
                    }
                    idx += num_4x4_w;
                }
                idy += num_4x4_h;
            }
            mi.mode = last_mode;
            mi.mv = mi.bmi_mv[3];
        } else {
            let mut best_ref_mvs = [(0i32, 0i32); 2];
            if mi.mode != ZEROMV {
                for r in 0..nrefs {
                    let frame = mi.ref_frame[r];
                    let (tmp, _count) = find_mv_refs(&self.mi, self.mi_cols, self.mi_rows, self.tile_col_start, self.tile_col_end, mi_row, mi_col, bsize, frame, &self.sign_bias, mi.mode, -1, edges, self.prev_mv(mi_row, mi_col));
                    // NEARESTMV / NEWMV use the nearest candidate (slot 0); NEARMV
                    // uses the near candidate (slot 1), which is the zero MV when no
                    // distinct second candidate was found (libvpx `mv_ref_list[1]`).
                    let idx = if mi.mode == NEARMV { 1 } else { 0 };
                    best_ref_mvs[r] = lower_mv_precision(tmp[idx], allow_hp);
                }
            }
            let mode = mi.mode;
            self.assign_mv(b, mode, &mut mi.mv, &best_ref_mvs, &best_ref_mvs, is_compound, allow_hp);
        }
    }

    /// `append_sub8x8_mvs_for_idx` — best ref MV for a sub-8×8 sub-block.
    #[allow(clippy::too_many_arguments)]
    fn append_sub8x8_mvs(&self, mi: &ModeInfo, b_mode: u8, block: usize, r: usize, mi_row: usize, mi_col: usize, bsize: usize, edges: (i32, i32, i32, i32)) -> Mv {
        let frame = mi.ref_frame[r];
        let find = |blk: i32| find_mv_refs(&self.mi, self.mi_cols, self.mi_rows, self.tile_col_start, self.tile_col_end, mi_row, mi_col, bsize, frame, &self.sign_bias, b_mode, blk, edges, self.prev_mv(mi_row, mi_col));
        match block {
            0 => {
                let (list, count) = find(0);
                list[count - 1]
            }
            1 | 2 => {
                if b_mode == NEARESTMV {
                    mi.bmi_mv[0][r]
                } else {
                    let (list, _) = find(block as i32);
                    let mut res = (0, 0);
                    for n in 0..2 {
                        if mi.bmi_mv[0][r] != list[n] {
                            res = list[n];
                            break;
                        }
                    }
                    res
                }
            }
            _ => {
                if b_mode == NEARESTMV {
                    mi.bmi_mv[2][r]
                } else if mi.bmi_mv[2][r] != mi.bmi_mv[1][r] {
                    mi.bmi_mv[1][r]
                } else if mi.bmi_mv[2][r] != mi.bmi_mv[0][r] {
                    mi.bmi_mv[0][r]
                } else {
                    let (list, _) = find(block as i32);
                    let mut res = (0, 0);
                    for n in 0..2 {
                        if mi.bmi_mv[2][r] != list[n] {
                            res = list[n];
                            break;
                        }
                    }
                    res
                }
            }
        }
    }

    fn assign_mv(&mut self, b: &mut BoolDecoder, mode: u8, mv: &mut [Mv; 2], ref_mv: &[Mv; 2], near_nearest: &[Mv; 2], is_compound: bool, allow_hp: bool) {
        match mode {
            NEWMV => {
                for i in 0..(1 + is_compound as usize) {
                    mv[i] = read_mv(b, ref_mv[i], &self.fc.nmvc, allow_hp, &mut self.counts.mv);
                }
            }
            NEARESTMV | NEARMV => {
                mv[0] = near_nearest[0];
                mv[1] = near_nearest[1];
            }
            _ => {
                // ZEROMV
                mv[0] = (0, 0);
                mv[1] = (0, 0);
            }
        }
    }

    fn read_switchable_interp_filter(&mut self, b: &mut BoolDecoder, above: Option<&ModeInfo>, left: Option<&ModeInfo>) -> u8 {
        let ctx = switchable_interp_context(above, left);
        // tree {-EIGHTTAP(0), 2, -EIGHTTAP_SMOOTH(1), -EIGHTTAP_SHARP(2)}
        const TREE: [i8; 4] = [0, 2, -1, -2];
        let t = crate::token::read_tree(b, &TREE, &self.fc.switchable_interp_prob[ctx]) as u8;
        self.counts.switchable_interp[ctx][t as usize] += 1;
        t
    }

    /// Block edge distances in 1/8-pel `(left, right, top, bottom)`.
    fn mb_to_edges(&self, mi_row: usize, mi_col: usize, bsize: usize) -> (i32, i32, i32, i32) {
        let bw8 = (1usize << B_WIDTH_LOG2[bsize] >> 1).max(1);
        let bh8 = (1usize << B_HEIGHT_LOG2[bsize] >> 1).max(1);
        let left = -((mi_col as i32 * 8) << 3);
        let right = (self.mi_cols as i32 - bw8 as i32 - mi_col as i32) * 8 * 8;
        let top = -((mi_row as i32 * 8) << 3);
        let bottom = (self.mi_rows as i32 - bh8 as i32 - mi_row as i32) * 8 * 8;
        (left, right, top, bottom)
    }

    // ---- Component 7: motion compensation --------------------------------

    /// Build the inter prediction for one plane of a coding block into the
    /// frame buffer (libvpx `dec_build_inter_predictors_sb`, non-scaled path).
    fn inter_predict_plane(&mut self, mi: &ModeInfo, plane: usize, mi_row: usize, mi_col: usize, bsize: usize, bwl: usize, bhl: usize) {
        let (ss_x, ss_y) = (self.planes[plane].ss_x, self.planes[plane].ss_y);
        let base_x = (mi_col * MI_SIZE) >> ss_x;
        let base_y = (mi_row * MI_SIZE) >> ss_y;
        let n4_w = (1usize << bwl) >> ss_x;
        let n4_h = (1usize << bhl) >> ss_y;
        let bw = (n4_w * 4) as i32;
        let bh = (n4_h * 4) as i32;
        let edges = self.mb_to_edges(mi_row, mi_col, bsize);
        let nrefs = 1 + mi.has_second_ref() as usize;
        for r in 0..nrefs {
            let frame = mi.ref_frame[r];
            let avg = r == 1;
            if (mi.sb_type as usize) < BLOCK_8X8 {
                let mut i = 0;
                for y in 0..n4_h {
                    for x in 0..n4_w {
                        let mv = average_split_mvs(mi, r, i, ss_x, ss_y);
                        self.mc_one(plane, frame, mv, base_x + x * 4, base_y + y * 4, 4, 4, bw, bh, ss_x, ss_y, edges, mi.interp_filter, avg);
                        i += 1;
                    }
                }
            } else {
                self.mc_one(plane, frame, mi.mv[r], base_x, base_y, n4_w * 4, n4_h * 4, bw, bh, ss_x, ss_y, edges, mi.interp_filter, avg);
            }
        }
    }

    #[allow(clippy::too_many_arguments)]
    fn mc_one(&mut self, plane: usize, frame: i8, mv: Mv, dst_x: usize, dst_y: usize, w: usize, h: usize, bw: i32, bh: i32, ss_x: usize, ss_y: usize, edges: (i32, i32, i32, i32), filter: u8, avg: bool) {
        let rf = match self.refs[(frame - LAST_FRAME) as usize].clone() {
            Some(rf) => rf,
            None => return, // missing reference (corrupt stream)
        };
        let mv_q4 = clamp_mv_umv(mv, bw, bh, ss_x, ss_y, edges);
        let stride = self.planes[plane].stride;
        let refp = RefPlane {
            buf: &rf.planes[plane],
            stride: rf.stride[plane],
            w: rf.w[plane] as i32,
            h: rf.h[plane] as i32,
        };
        let dst_off = dst_y * stride + dst_x;
        // Reference scaling: a reference coded at a different resolution needs the
        // scaled convolve (libvpx `dec_build_inter_predictors`, scaled branch).
        // The scale factor comes from the *luma* frame dimensions and applies to
        // every plane.
        let scaled = rf.w[0] != self.planes[0].width || rf.h[0] != self.planes[0].height;
        if scaled {
            let cur_w = self.planes[0].width as i64;
            let cur_h = self.planes[0].height as i64;
            let x_scale_fp = ((rf.w[0] as i64) << 14) / cur_w;
            let y_scale_fp = ((rf.h[0] as i64) << 14) / cur_h;
            let sx = |v: i64| ((v * x_scale_fp) >> 14) as i32;
            let sy = |v: i64| ((v * y_scale_fp) >> 14) as i32;
            let x_step_q4 = sx(16);
            let y_step_q4 = sy(16);
            // Block origin scaled into the reference plane.
            let x0 = sx(dst_x as i64);
            let y0 = sy(dst_y as i64);
            // `vp9_scale_mv`: scale the MV and add the sub-pixel offset from
            // scaling the block's *luma* position (`mi_x + x`, here `dst<<ss`).
            let lx = (dst_x << ss_x) as i64;
            let ly = (dst_y << ss_y) as i64;
            let x_off = sx(lx << 4) & 15;
            let y_off = sy(ly << 4) & 15;
            let smv_col = sx(mv_q4.1 as i64) + x_off;
            let smv_row = sy(mv_q4.0 as i64) + y_off;
            let bx = x0 + (smv_col >> 4);
            let by = y0 + (smv_row >> 4);
            scaled_predict_block(
                &refp, bx, by, (smv_col & 15) as usize, (smv_row & 15) as usize, x_step_q4, y_step_q4,
                filter as usize, &mut self.planes[plane].buf[dst_off..], stride, w, h, avg, self.max_px,
            );
        } else {
            let bx = dst_x as i32 + (mv_q4.1 >> 4);
            let by = dst_y as i32 + (mv_q4.0 >> 4);
            let subpel_x = (mv_q4.1 & 15) as usize;
            let subpel_y = (mv_q4.0 & 15) as usize;
            predict_block(&refp, bx, by, subpel_x, subpel_y, filter as usize, &mut self.planes[plane].buf[dst_off..], stride, w, h, avg, self.max_px);
        }
    }

    #[allow(clippy::too_many_arguments)]
    fn reconstruct_plane(&mut self, b: &mut BoolDecoder, mi: &ModeInfo, plane: usize, mi_row: usize, mi_col: usize, bsize: usize, bwl: usize, bhl: usize) -> crate::Result<()> {
        if mi.is_inter {
            self.inter_predict_plane(mi, plane, mi_row, mi_col, bsize, bwl, bhl);
        }
        let (ss_x, ss_y) = (self.planes[plane].ss_x, self.planes[plane].ss_y);
        // Plane block geometry in 4×4 units, from the partition level (libvpx
        // set_plane_n4) — for sub-8×8 this differs from num_4x4[bsize].
        let n4_w = (1usize << bwl) >> ss_x;
        let n4_h = (1usize << bhl) >> ss_y;
        let tx_size = if plane == 0 { mi.tx_size as usize } else { uv_tx_size(bsize, mi.tx_size as usize, ss_x, ss_y) };
        let step = 1usize << tx_size;

        // Frame-edge clipping of the transform-block grid (libvpx max_blocks_*).
        let bw_mi = 1usize << (bwl - 1); // mi-unit width of the block
        let bh_mi = 1usize << (bhl - 1);
        let mb_to_right = (self.mi_cols as i32 - bw_mi as i32 - mi_col as i32) * (MI_SIZE as i32) * 8;
        let mb_to_bottom = (self.mi_rows as i32 - bh_mi as i32 - mi_row as i32) * (MI_SIZE as i32) * 8;
        let max_w = if mb_to_right >= 0 { n4_w } else { (n4_w as i32 + (mb_to_right >> (5 + ss_x))).max(0) as usize };
        let max_h = if mb_to_bottom >= 0 { n4_h } else { (n4_h as i32 + (mb_to_bottom >> (5 + ss_y))).max(0) as usize };

        let above_some = self.above_mi(mi_row, mi_col).is_some();
        let left_some = self.left_mi(mi_row, mi_col).is_some();

        // Plane pixel origin of this coding block.
        let base_x = (mi_col * MI_SIZE) >> ss_x;
        let base_y = (mi_row * MI_SIZE) >> ss_y;
        let above_col0 = (mi_col * 2) >> ss_x; // 4×4 column in the above-context array
        let left_row0 = ((mi_row * 2) & 15) >> ss_y;

        let mut row = 0;
        while row < max_h {
            let mut col = 0;
            while col < max_w {
                self.reconstruct_tx_block(
                    b, mi, plane, tx_size, n4_w, row, col, base_x, base_y, above_col0, left_row0,
                    above_some, left_some, max_w, max_h, mb_to_right, mb_to_bottom,
                )?;
                col += step;
            }
            row += step;
        }
        Ok(())
    }

    #[allow(clippy::too_many_arguments)]
    fn reconstruct_tx_block(
        &mut self,
        b: &mut BoolDecoder,
        mi: &ModeInfo,
        plane: usize,
        tx_size: usize,
        n4_w: usize,
        row: usize,
        col: usize,
        base_x: usize,
        base_y: usize,
        above_col0: usize,
        left_row0: usize,
        above_some: bool,
        left_some: bool,
        max_w: usize,
        max_h: usize,
        mb_to_right: i32,
        mb_to_bottom: i32,
    ) -> crate::Result<()> {
        let txw = 1usize << tx_size; // 4×4 units
        let bs = 4usize << tx_size; // pixels
        let stride = self.planes[plane].stride;
        let fw = self.planes[plane].width as i32;
        let fh = self.planes[plane].height as i32;

        let x0 = base_x + col * 4;
        let y0 = base_y + row * 4;
        let dst_off = y0 * stride + x0;

        // Intra prediction (inter blocks are already motion-compensated).
        let mut intra_mode = 0u8;
        if !mi.is_inter {
            let mode = if plane == 0 {
                if (mi.sb_type as usize) < BLOCK_8X8 {
                    mi.bmi[(row << 1) + col]
                } else {
                    mi.mode
                }
            } else {
                mi.uv_mode
            };
            intra_mode = mode;
            let up_avail = row > 0 || above_some;
            let left_avail = col > 0 || left_some;
            let right_avail = (col + txw) < n4_w;
            let mut above_buf = [0u16; 1 + 64];
            let mut left_buf = [0u16; 32];
            build_intra_edges(
                mode, bs, up_avail, left_avail, right_avail, &self.planes[plane].buf, stride, fw, fh,
                x0 as i32, y0 as i32, mb_to_right, mb_to_bottom, &mut above_buf, &mut left_buf, self.max_px,
            );
            predict(&mut self.planes[plane].buf[dst_off..], stride, mode, bs, &above_buf, &left_buf, left_avail, up_avail, self.max_px);
        }

        let inframe_w = (max_w - col).min(txw);
        let inframe_h = (max_h - row).min(txw);

        if mi.skip {
            // Skipped: no residual; clear the entropy context for this block.
            self.set_ctx(plane, above_col0 + col, left_row0 + row, txw, txw, inframe_w, inframe_h, false);
            return Ok(());
        }

        // Token decode + dequant. Inter / lossless / chroma / 32×32 use DCT_DCT;
        // only ≤16×16 intra luma uses the mode-derived hybrid transform.
        let tx_type = if mi.is_inter || self.lossless || plane != 0 || tx_size == 3 {
            TxType::DctDct
        } else {
            INTRA_MODE_TO_TX_TYPE[intra_mode as usize]
        };
        let (scan, nb) = get_scan(tx_size, tx_type);
        let sid = mi.segment_id as usize;
        let dq = if plane == 0 { self.dq_y[sid] } else { self.dq_uv[sid] };

        let act = self.above_ctx_val(plane, above_col0 + col, txw);
        let lct = self.left_ctx_val(plane, left_row0 + row, txw);
        let ctx0 = (act + lct) as usize;

        let pt = plane.min(1);
        let rt = mi.is_inter as usize;
        let bd_bits = (self.max_px as u32 + 1).trailing_zeros();
        let (eob, max_row) = decode_coefs(
            b, &self.fc.coef_probs[tx_size][pt][rt], tx_size, scan, nb, dq, ctx0,
            &mut self.dqcoeff, &mut self.token_cache,
            &mut self.counts.coef[tx_size][pt][rt], &mut self.counts.eob_branch[tx_size][pt][rt],
            bd_bits,
        );

        self.set_ctx(plane, above_col0 + col, left_row0 + row, txw, txw, inframe_w, inframe_h, eob > 0);

        if eob > 0 {
            let dst = &mut self.planes[plane].buf[dst_off..];
            if self.lossless {
                inverse_wht_add(&self.dqcoeff, dst, stride, self.max_px);
            } else if eob == 1 && tx_type == TxType::DctDct {
                // DC-only: a single flat offset (bit-exact O(1) path).
                inverse_transform_dc_add(self.dqcoeff[0], bs, dst, stride, self.max_px);
            } else {
                // Sparse-EOB: only rows 0..=max_row hold non-zero coefficients.
                inverse_transform_add_rows(&self.dqcoeff, bs, tx_type, dst, stride, self.max_px, max_row + 1);
            }
        }
        Ok(())
    }

    fn above_ctx_val(&self, plane: usize, idx: usize, txw: usize) -> u8 {
        self.above_ctx[plane][idx..idx + txw].iter().any(|&v| v != 0) as u8
    }
    fn left_ctx_val(&self, plane: usize, idx: usize, txh: usize) -> u8 {
        self.left_ctx[plane][idx..idx + txh].iter().any(|&v| v != 0) as u8
    }

    /// Write the above/left entropy context for a decoded transform block.
    /// The above row is trimmed to the in-frame columns and the left column to
    /// the in-frame rows (libvpx ctx_shift) so out-of-frame units stay zero.
    #[allow(clippy::too_many_arguments)]
    fn set_ctx(&mut self, plane: usize, above_idx: usize, left_idx: usize, txw: usize, txh: usize, inframe_w: usize, inframe_h: usize, nonzero: bool) {
        let v = nonzero as u8;
        for i in 0..txw {
            self.above_ctx[plane][above_idx + i] = if i < inframe_w { v } else { 0 };
        }
        for i in 0..txh {
            self.left_ctx[plane][left_idx + i] = if i < inframe_h { v } else { 0 };
        }
    }
}

/// Chroma transform size from the luma tx size and subsampling
/// (libvpx `get_uv_tx_size_impl`): sub-8×8 luma → chroma TX_4X4; otherwise the
/// luma tx clamped to the largest tx of the subsampled (chroma) block size.
fn uv_tx_size(bsize: usize, tx_size: usize, ss_x: usize, ss_y: usize) -> usize {
    if bsize < BLOCK_8X8 {
        return 0;
    }
    tx_size.min(MAX_TXSIZE[ss_size_lookup(bsize, ss_x, ss_y)] as usize)
}

/// Map a luma block size to its chroma block size under subsampling
/// (libvpx `ss_size_lookup`), computed from the 4×4-unit dimensions.
pub(crate) fn ss_size_lookup(bsize: usize, ss_x: usize, ss_y: usize) -> usize {
    if ss_x == 0 && ss_y == 0 {
        return bsize;
    }
    const N4W: [usize; 13] = [1, 1, 2, 2, 2, 4, 4, 4, 8, 8, 8, 16, 16];
    const N4H: [usize; 13] = [1, 2, 1, 2, 4, 2, 4, 8, 4, 8, 16, 8, 16];
    let w = (N4W[bsize] >> ss_x).max(1);
    let h = (N4H[bsize] >> ss_y).max(1);
    (0..13).find(|&b| N4W[b] == w && N4H[b] == h).unwrap_or(0)
}
