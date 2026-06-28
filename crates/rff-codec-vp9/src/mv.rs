//! VP9 motion-vector entropy decode (ISO/VP9 §9.3.2 / libvpx
//! `read_mv` + `read_mv_component`, `vp9_entropymv.c`).
//!
//! Component 3 — decodes an MV difference from the boolean stream using the
//! `nmv_context` model (joint type, then per-component sign / class / integer
//! bits / fractional / high-precision), and adds it to the reference MV.
// Wired into the inter mode-info decoder (next).
#![allow(dead_code)]

use crate::bits::BoolDecoder;
use crate::prob_tables::{NmvComp, NmvContext};
use crate::token::read_tree;

// MV trees (libvpx `vp9_entropymv.c`). Leaves are non-positive; `-0 == 0`.
pub(crate) const MV_JOINT_TREE: [i8; 6] = [0, 2, -1, 4, -2, -3];
pub(crate) const MV_CLASS_TREE: [i8; 20] = [
    0, 2, -1, 4, 6, 8, -2, -3, 10, 12, -4, -5, -6, 14, 16, 18, -7, -8, -9, -10,
];
pub(crate) const MV_FP_TREE: [i8; 6] = [0, 2, -1, 4, -2, -3];

const CLASS0_BITS: i32 = 1;
const CLASS0_SIZE: i32 = 2;

/// High precision is used only when the reference MV is small (`use_mv_hp`).
fn use_mv_hp(ref_mv: (i32, i32)) -> bool {
    ref_mv.0.abs() < 64 && ref_mv.1.abs() < 64
}

/// One MV component's symbol counts (`nmv_component_counts`).
#[derive(Clone, Default, PartialEq, Eq)]
pub(crate) struct NmvCompCounts {
    pub sign: [u32; 2],
    pub classes: [u32; 11],
    pub class0: [u32; 2],
    pub bits: [[u32; 2]; 10],
    pub class0_fp: [[u32; 4]; 2],
    pub fp: [u32; 4],
    pub class0_hp: [u32; 2],
    pub hp: [u32; 2],
}

/// MV entropy counts (`nmv_context_counts`).
#[derive(Clone, Default, PartialEq, Eq)]
pub(crate) struct NmvCounts {
    pub joints: [u32; 4],
    pub comps: [NmvCompCounts; 2],
}

use crate::decode::CountAdd;
impl CountAdd for NmvCounts {
    fn merge(&mut self, o: &Self) {
        self.joints.merge(&o.joints);
        self.comps.merge(&o.comps);
    }
}
impl CountAdd for NmvCompCounts {
    fn merge(&mut self, o: &Self) {
        self.sign.merge(&o.sign);
        self.classes.merge(&o.classes);
        self.class0.merge(&o.class0);
        self.bits.merge(&o.bits);
        self.class0_fp.merge(&o.class0_fp);
        self.fp.merge(&o.fp);
        self.class0_hp.merge(&o.class0_hp);
        self.hp.merge(&o.hp);
    }
}

/// Decode one MV component difference (`read_mv_component`), counting symbols.
fn read_mv_component(
    b: &mut BoolDecoder,
    c: &NmvComp,
    usehp: bool,
    cnt: &mut NmvCompCounts,
) -> i32 {
    let sign = b.read_bool(c.sign) != 0;
    cnt.sign[sign as usize] += 1;
    let mv_class = read_tree(b, &MV_CLASS_TREE, &c.classes); // 0..=10
    cnt.classes[mv_class as usize] += 1;
    let class0 = mv_class == 0;

    let d;
    let mut mag;
    if class0 {
        d = b.read_bool(c.class0[0]) as i32;
        cnt.class0[d as usize] += 1;
        mag = 0;
    } else {
        let n = mv_class + CLASS0_BITS - 1; // number of integer bits
        let mut acc = 0i32;
        for i in 0..n {
            let bit = b.read_bool(c.bits[i as usize]) as i32;
            cnt.bits[i as usize][bit as usize] += 1;
            acc |= bit << i;
        }
        d = acc;
        mag = CLASS0_SIZE << (mv_class + 2);
    }

    let fp = if class0 {
        read_tree(b, &MV_FP_TREE, &c.class0_fp[d as usize])
    } else {
        read_tree(b, &MV_FP_TREE, &c.fp)
    };
    if class0 {
        cnt.class0_fp[d as usize][fp as usize] += 1;
    } else {
        cnt.fp[fp as usize] += 1;
    }
    let hp = if usehp {
        b.read_bool(if class0 { c.class0_hp } else { c.hp }) as i32
    } else {
        1
    };
    if class0 {
        cnt.class0_hp[hp as usize] += 1;
    } else {
        cnt.hp[hp as usize] += 1;
    }

    mag += ((d << 3) | (fp << 1) | hp) + 1;
    if sign {
        -mag
    } else {
        mag
    }
}

/// Decode an MV: joint type, then the present components, added to `ref_mv`.
/// MVs are in 1/8-pel units. Returns `(row, col)`. Accumulates entropy counts.
pub fn read_mv(
    b: &mut BoolDecoder,
    ref_mv: (i32, i32),
    ctx: &NmvContext,
    allow_hp: bool,
    cnt: &mut NmvCounts,
) -> (i32, i32) {
    let joint = read_tree(b, &MV_JOINT_TREE, &ctx.joints); // 0..=3
    cnt.joints[joint as usize] += 1;
    let use_hp = allow_hp && use_mv_hp(ref_mv);
    let mut diff = (0i32, 0i32);
    // mv_joint_vertical: HZVNZ(2) or HNZVNZ(3).
    if joint == 2 || joint == 3 {
        diff.0 = read_mv_component(b, &ctx.comps[0], use_hp, &mut cnt.comps[0]);
    }
    // mv_joint_horizontal: HNZVZ(1) or HNZVNZ(3).
    if joint == 1 || joint == 3 {
        diff.1 = read_mv_component(b, &ctx.comps[1], use_hp, &mut cnt.comps[1]);
    }
    (ref_mv.0 + diff.0, ref_mv.1 + diff.1)
}

// ---- Component 5: find_mv_refs (spatial MV candidate scan) --------------

use crate::block::{ModeInfo, Mv, BLOCK_8X8, INTRA_FRAME, NEARMV};

/// Per-mi motion record kept for the next frame's temporal MV prediction
/// (libvpx `MV_REF`).
#[derive(Clone, Copy, Default)]
pub(crate) struct MvRef {
    pub ref_frame: [i8; 2],
    pub mv: [Mv; 2],
}

/// `mv_ref_blocks[BLOCK_SIZES][MVREF_NEIGHBOURS]` — (row, col) neighbour scan
/// pattern per block size (libvpx `vp9_mvref_common.h`).
const MV_REF_BLOCKS: [[(i32, i32); 8]; 13] = [
    // 4X4 / 4X8 / 8X4 / 8X8 share the same pattern.
    [
        (-1, 0),
        (0, -1),
        (-1, -1),
        (-2, 0),
        (0, -2),
        (-2, -1),
        (-1, -2),
        (-2, -2),
    ],
    [
        (-1, 0),
        (0, -1),
        (-1, -1),
        (-2, 0),
        (0, -2),
        (-2, -1),
        (-1, -2),
        (-2, -2),
    ],
    [
        (-1, 0),
        (0, -1),
        (-1, -1),
        (-2, 0),
        (0, -2),
        (-2, -1),
        (-1, -2),
        (-2, -2),
    ],
    [
        (-1, 0),
        (0, -1),
        (-1, -1),
        (-2, 0),
        (0, -2),
        (-2, -1),
        (-1, -2),
        (-2, -2),
    ],
    // 8X16
    [
        (0, -1),
        (-1, 0),
        (1, -1),
        (-1, -1),
        (0, -2),
        (-2, 0),
        (-2, -1),
        (-1, -2),
    ],
    // 16X8
    [
        (-1, 0),
        (0, -1),
        (-1, 1),
        (-1, -1),
        (-2, 0),
        (0, -2),
        (-1, -2),
        (-2, -1),
    ],
    // 16X16
    [
        (-1, 0),
        (0, -1),
        (-1, 1),
        (1, -1),
        (-1, -1),
        (-3, 0),
        (0, -3),
        (-3, -3),
    ],
    // 16X32
    [
        (0, -1),
        (-1, 0),
        (2, -1),
        (-1, -1),
        (-1, 1),
        (0, -3),
        (-3, 0),
        (-3, -3),
    ],
    // 32X16
    [
        (-1, 0),
        (0, -1),
        (-1, 2),
        (-1, -1),
        (1, -1),
        (-3, 0),
        (0, -3),
        (-3, -3),
    ],
    // 32X32
    [
        (-1, 1),
        (1, -1),
        (-1, 2),
        (2, -1),
        (-1, -1),
        (-3, 0),
        (0, -3),
        (-3, -3),
    ],
    // 32X64
    [
        (0, -1),
        (-1, 0),
        (4, -1),
        (-1, 2),
        (-1, -1),
        (0, -3),
        (-3, 0),
        (2, -1),
    ],
    // 64X32
    [
        (-1, 0),
        (0, -1),
        (-1, 4),
        (2, -1),
        (-1, -1),
        (-3, 0),
        (0, -3),
        (-1, 2),
    ],
    // 64X64
    [
        (-1, 3),
        (3, -1),
        (-1, 4),
        (4, -1),
        (-1, -1),
        (-1, 0),
        (0, -1),
        (-1, 6),
    ],
];

const IDX_N_COLUMN_TO_SUBBLOCK: [[usize; 2]; 4] = [[1, 2], [1, 3], [3, 2], [3, 3]];

const MODE_2_COUNTER: [i32; 14] = [9, 9, 9, 9, 9, 9, 9, 9, 9, 9, 0, 0, 3, 1];
const COUNTER_TO_CONTEXT: [u8; 19] = [2, 3, 4, 1, 3, 9, 0, 9, 9, 5, 5, 9, 5, 9, 9, 9, 9, 9, 6];

const MV_BORDER: i32 = 16 << 3; // 1/8-pel border for ref-MV clamping

/// Edge distances in 1/8-pel: `(left, right, top, bottom)`.
pub type Edges = (i32, i32, i32, i32);

#[inline]
fn is_inside(
    mi_col: usize,
    mi_row: usize,
    col_start: usize,
    col_end: usize,
    mi_rows: usize,
    p: (i32, i32),
) -> bool {
    let r = mi_row as i32 + p.0;
    let c = mi_col as i32 + p.1;
    // Neighbours must stay within the frame rows and the current tile's columns.
    r >= 0 && c >= col_start as i32 && (r as usize) < mi_rows && (c as usize) < col_end
}

#[inline]
fn clamp_mv_ref(mv: Mv, e: Edges) -> Mv {
    (
        mv.0.clamp(e.2 - MV_BORDER, e.3 + MV_BORDER),
        mv.1.clamp(e.0 - MV_BORDER, e.1 + MV_BORDER),
    )
}

fn get_sub_block_mv(cand: &ModeInfo, which: usize, search_col: i32, block: i32) -> Mv {
    if block >= 0 && (cand.sb_type as usize) < BLOCK_8X8 {
        cand.bmi_mv[IDX_N_COLUMN_TO_SUBBLOCK[block as usize][(search_col == 0) as usize]][which]
    } else {
        cand.mv[which]
    }
}

fn scale_mv(cand: &ModeInfo, ref_idx: usize, this_ref: i8, sign_bias: &[bool; 4]) -> Mv {
    let mv = cand.mv[ref_idx];
    if sign_bias[cand.ref_frame[ref_idx] as usize] != sign_bias[this_ref as usize] {
        (-mv.0, -mv.1)
    } else {
        mv
    }
}

/// `get_mode_context` — entropy context for the inter-mode read, from the two
/// nearest neighbours' modes.
#[allow(clippy::too_many_arguments)]
pub fn get_mode_context(
    mi: &[ModeInfo],
    mi_stride: usize,
    mi_rows: usize,
    col_start: usize,
    col_end: usize,
    mi_row: usize,
    mi_col: usize,
    bsize: usize,
) -> usize {
    let search = &MV_REF_BLOCKS[bsize];
    let mut counter = 0i32;
    for &p in search.iter().take(2) {
        if is_inside(mi_col, mi_row, col_start, col_end, mi_rows, p) {
            let idx = ((mi_row as i32 + p.0) as usize) * mi_stride + (mi_col as i32 + p.1) as usize;
            counter += MODE_2_COUNTER[mi[idx].mode as usize];
        }
    }
    COUNTER_TO_CONTEXT[counter as usize] as usize
}

/// `dec_find_mv_refs` — collect up to two reference-MV candidates for `ref_frame`
/// from the spatial neighbourhood. Returns `(mv_list, refmv_count)`.
/// Temporal (previous-frame) candidates are omitted (`use_prev_frame_mvs=false`).
#[allow(clippy::too_many_arguments)]
pub fn find_mv_refs(
    mi: &[ModeInfo],
    mi_stride: usize,
    mi_rows: usize,
    col_start: usize,
    col_end: usize,
    mi_row: usize,
    mi_col: usize,
    bsize: usize,
    ref_frame: i8,
    sign_bias: &[bool; 4],
    mode: u8,
    block: i32,
    edges: Edges,
    prev: Option<&MvRef>,
) -> ([Mv; 2], usize) {
    let search = &MV_REF_BLOCKS[bsize];
    let early_break = mode != NEARMV;
    let mut list = [(0i32, 0i32); 2];
    let mut count = 0usize;
    let mut diff = false;

    // Returns true if the candidate list is full (stop scanning).
    let add = |mv: Mv, list: &mut [Mv; 2], count: &mut usize| -> bool {
        if *count > 0 {
            if mv != list[0] {
                list[*count] = mv;
                *count += 1;
                return true;
            }
            false
        } else {
            list[*count] = mv;
            *count += 1;
            early_break
        }
    };
    let cand_at = |p: (i32, i32)| -> usize {
        ((mi_row as i32 + p.0) as usize) * mi_stride + (mi_col as i32 + p.1) as usize
    };

    'scan: {
        let mut i = 0;
        if block >= 0 {
            while i < 2 {
                let p = search[i];
                if is_inside(mi_col, mi_row, col_start, col_end, mi_rows, p) {
                    let cand = &mi[cand_at(p)];
                    diff = true;
                    if cand.ref_frame[0] == ref_frame {
                        if add(get_sub_block_mv(cand, 0, p.1, block), &mut list, &mut count) {
                            break 'scan;
                        }
                    } else if cand.ref_frame[1] == ref_frame
                        && add(get_sub_block_mv(cand, 1, p.1, block), &mut list, &mut count)
                    {
                        break 'scan;
                    }
                }
                i += 1;
            }
        }
        while i < 8 {
            let p = search[i];
            if is_inside(mi_col, mi_row, col_start, col_end, mi_rows, p) {
                let cand = &mi[cand_at(p)];
                diff = true;
                if cand.ref_frame[0] == ref_frame {
                    if add(cand.mv[0], &mut list, &mut count) {
                        break 'scan;
                    }
                } else if cand.ref_frame[1] == ref_frame && add(cand.mv[1], &mut list, &mut count) {
                    break 'scan;
                }
            }
            i += 1;
        }
        // Temporal (previous-frame) candidate at the same mi position, same ref.
        if let Some(p) = prev {
            if p.ref_frame[0] == ref_frame {
                if add(p.mv[0], &mut list, &mut count) {
                    break 'scan;
                }
            } else if p.ref_frame[1] == ref_frame && add(p.mv[1], &mut list, &mut count) {
                break 'scan;
            }
        }
        if diff {
            for &p in search.iter() {
                if is_inside(mi_col, mi_row, col_start, col_end, mi_rows, p) {
                    let cand = &mi[cand_at(p)];
                    if cand.is_inter_block() {
                        if cand.ref_frame[0] != ref_frame
                            && add(
                                scale_mv(cand, 0, ref_frame, sign_bias),
                                &mut list,
                                &mut count,
                            )
                        {
                            break 'scan;
                        }
                        if cand.has_second_ref()
                            && cand.ref_frame[1] != ref_frame
                            && cand.mv[1] != cand.mv[0]
                            && add(
                                scale_mv(cand, 1, ref_frame, sign_bias),
                                &mut list,
                                &mut count,
                            )
                        {
                            break 'scan;
                        }
                    }
                }
            }
        }
        // Temporal candidates from different reference frames (sign-flipped).
        if let Some(p) = prev {
            let flip = |mv: Mv, rf: i8| -> Mv {
                if sign_bias[rf as usize] != sign_bias[ref_frame as usize] {
                    (-mv.0, -mv.1)
                } else {
                    mv
                }
            };
            if p.ref_frame[0] != ref_frame && p.ref_frame[0] > INTRA_FRAME {
                if add(flip(p.mv[0], p.ref_frame[0]), &mut list, &mut count) {
                    break 'scan;
                }
            }
            if p.ref_frame[1] > INTRA_FRAME
                && p.ref_frame[1] != ref_frame
                && p.mv[1] != p.mv[0]
                && add(flip(p.mv[1], p.ref_frame[1]), &mut list, &mut count)
            {
                break 'scan;
            }
        }
    }

    let refmv_count = if mode == NEARMV { 2 } else { 1 };
    for m in list.iter_mut().take(refmv_count) {
        *m = clamp_mv_ref(*m, edges);
    }
    (list, refmv_count)
}

/// `lower_mv_precision` — drop the 1/8-pel bit when high precision is unused.
pub fn lower_mv_precision(mv: Mv, allow_hp: bool) -> Mv {
    let use_hp = allow_hp && use_mv_hp(mv);
    let mut mv = mv;
    if !use_hp {
        if mv.0 & 1 != 0 {
            mv.0 += if mv.0 > 0 { -1 } else { 1 };
        }
        if mv.1 & 1 != 0 {
            mv.1 += if mv.1 > 0 { -1 } else { 1 };
        }
    }
    mv
}

/// `is_mv_valid` — within the 14-bit usable MV range.
pub fn is_mv_valid(mv: (i32, i32)) -> bool {
    const MV_UPP: i32 = (1 << 14) - 1;
    const MV_LOW: i32 = -(1 << 14);
    mv.0 > MV_LOW && mv.0 < MV_UPP && mv.1 > MV_LOW && mv.1 < MV_UPP
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::prob_tables::DEFAULT_NMV_CONTEXT;

    #[test]
    fn trees_terminate_and_range() {
        // Drive the decoder over arbitrary bytes; every decode must terminate
        // and land a valid joint/class and an in-range component.
        for seed in 0..32u8 {
            let bytes = [
                seed,
                seed ^ 0x5a,
                0x13,
                0xC4,
                0x77,
                seed.wrapping_mul(3),
                0x01,
                0xFE,
            ];
            let mut b = BoolDecoder::new(&bytes).unwrap();
            let mv = read_mv(
                &mut b,
                (4, -8),
                &DEFAULT_NMV_CONTEXT,
                true,
                &mut NmvCounts::default(),
            );
            // Component magnitudes are bounded; just assert the call returns.
            let _ = mv;
        }
    }

    #[test]
    fn zero_joint_keeps_reference() {
        // Construct a stream whose first joint-tree read yields MV_JOINT_ZERO.
        // joints[0] is small (32) so the high-probability branch (bit 0) is the
        // common path; a 0x00-leading buffer decodes bit 0 first -> ZERO.
        let bytes = [0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00];
        let mut b = BoolDecoder::new(&bytes).unwrap();
        let mv = read_mv(
            &mut b,
            (10, 20),
            &DEFAULT_NMV_CONTEXT,
            false,
            &mut NmvCounts::default(),
        );
        assert_eq!(mv, (10, 20));
    }
}
