//! VP9 in-loop deblocking filter (ISO/VP9 §8.8 / libvpx `vp9_loopfilter.c` +
//! `vpx_dsp/loopfilter.c`). Applied to the reconstructed frame after intra/inter
//! reconstruction, before the frame is output or used as a reference.
//!
//! This is the general (`non420`) per-plane path, which is correct for every
//! subsampling. Filtering proceeds superblock by superblock: for each 64×64 it
//! builds per-edge masks (16/8/4-wide and the internal 4×4 edge) from each
//! block's transform size, then applies the vertical edges followed by the
//! horizontal edges. The leaf kernels (`filter4/8/16`) are transcribed verbatim.

use crate::block::ModeInfo;
use crate::geom_tables::MAX_TXSIZE;
use crate::FrameHeader;

const MAX_LOOP_FILTER: usize = 63;
const MI_BLOCK_SIZE: usize = 8; // mi units per superblock side

// Block geometry in 4×4 / 8×8 units, indexed by BLOCK_SIZE.
const NUM_4X4_W: [usize; 13] = [1, 1, 2, 2, 2, 4, 4, 4, 8, 8, 8, 16, 16];
const NUM_4X4_H: [usize; 13] = [1, 2, 1, 2, 4, 2, 4, 8, 4, 8, 16, 8, 16];
const NUM_8X8_W: [usize; 13] = [1, 1, 1, 1, 1, 2, 2, 2, 4, 4, 4, 8, 8];
const NUM_8X8_H: [usize; 13] = [1, 1, 1, 1, 2, 1, 2, 4, 2, 4, 8, 4, 8];

/// `mode_lf_lut`: maps a prediction mode to its loop-filter mode-delta class.
/// All intra modes are class 0; for inter, ZEROMV is 0 and the rest are 1.
const MODE_LF_LUT: [u8; 14] = [0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 1, 1, 0, 1];

/// Per-level thresholds (`loop_filter_thresh`).
#[derive(Clone, Copy, Default)]
struct Thresh {
    // Pre-scaled by `<<(bd-8)` so the leaf masks compare directly (i32 because a
    // 12-bit `mblim` exceeds 8 bits).
    mblim: i32,
    lim: i32,
    hev_thr: i32,
}

/// All sharpness-derived thresholds plus the per-(segment,ref,mode) levels.
struct LfInfo {
    thr: [Thresh; MAX_LOOP_FILTER + 1],
    // lvl[seg][ref][mode_class]; only seg 0 / ref 0 used on a key frame.
    lvl: [[[u8; 2]; 4]; 8],
}

fn build_lf_info(h: &FrameHeader) -> LfInfo {
    let sharpness = h.loop_filter_sharpness as i32;
    // Threshold scale for high bit depth (`0` at 8-bit).
    let shift = (h.bit_depth.max(8) as i32) - 8;
    let mut thr = [Thresh::default(); MAX_LOOP_FILTER + 1];
    for (lvl, t) in thr.iter_mut().enumerate() {
        let lvl = lvl as i32;
        let mut bil = lvl >> ((sharpness > 0) as i32 + (sharpness > 4) as i32);
        if sharpness > 0 && bil > 9 - sharpness {
            bil = 9 - sharpness;
        }
        if bil < 1 {
            bil = 1;
        }
        t.lim = bil << shift;
        t.mblim = (2 * (lvl + 2) + bil) << shift;
        t.hev_thr = (lvl >> 4) << shift;
    }

    // Per-segment / ref / mode levels (`vp9_loop_filter_frame_init`).
    let base = h.loop_filter_level as i32;
    let scale = 1 << (base >> 5);
    let mut lvl = [[[0u8; 2]; 4]; 8];
    for (seg_id, seg) in lvl.iter_mut().enumerate() {
        // Per-segment base level via SEG_LVL_ALT_LF (feature index 1).
        let mut lvl_seg = base;
        if h.seg_enabled && h.seg_feature_enabled[seg_id][1] {
            let data = h.seg_feature_data[seg_id][1];
            lvl_seg = (if h.seg_abs_delta { data } else { base + data }).clamp(0, MAX_LOOP_FILTER as i32);
        }
        if !h.lf_delta_enabled {
            for refs in seg.iter_mut() {
                refs[0] = lvl_seg as u8;
                refs[1] = lvl_seg as u8;
            }
        } else {
            let intra = (lvl_seg + h.lf_ref_deltas[0] * scale).clamp(0, MAX_LOOP_FILTER as i32);
            seg[0][0] = intra as u8;
            for r in 1..4 {
                for m in 0..2 {
                    let v = lvl_seg + h.lf_ref_deltas[r] * scale + h.lf_mode_deltas[m] * scale;
                    seg[r][m] = v.clamp(0, MAX_LOOP_FILTER as i32) as u8;
                }
            }
        }
    }
    LfInfo { thr, lvl }
}

fn get_filter_level(lf: &LfInfo, mi: &ModeInfo) -> u8 {
    // Indexed by the block's reference (INTRA=0, LAST=1, GOLDEN=2, ALTREF=3) so
    // GOLDEN/ALTREF blocks pick up their own ref-delta level, not LAST's.
    let refidx = if mi.is_inter { (mi.ref_frame[0] as usize).clamp(1, 3) } else { 0 };
    lf.lvl[mi.segment_id as usize][refidx][MODE_LF_LUT[mi.mode as usize] as usize]
}

/// Chroma/luma transform size for a plane (4:4:4 / luma → identity).
fn plane_tx_size(mi: &ModeInfo, ss_x: usize, ss_y: usize) -> usize {
    let tx = mi.tx_size as usize;
    if ss_x == 0 && ss_y == 0 {
        return tx;
    }
    // uv_txsize: clamp to the max tx of the subsampled block size.
    let ss_bsize = ss_size_lookup(mi.sb_type as usize, ss_x, ss_y);
    tx.min(MAX_TXSIZE[ss_bsize] as usize)
}

/// `ss_size_lookup[bsize][ss_x][ss_y]` for the common 4:2:0/4:2:2/4:4:4 cases.
fn ss_size_lookup(bsize: usize, ss_x: usize, ss_y: usize) -> usize {
    // Map a luma block size to the chroma block size under subsampling.
    // Derived from libvpx ss_size_lookup; BLOCK_INVALID cases clamp to 4x4.
    const BLOCK_4X4: usize = 0;
    if ss_x == 0 && ss_y == 0 {
        return bsize;
    }
    // num 4x4 reduced by subsampling, recomposed to a block size.
    let w = (NUM_4X4_W[bsize] >> ss_x).max(1);
    let hgt = (NUM_4X4_H[bsize] >> ss_y).max(1);
    // Find the block size with these 4x4 dims.
    for b in 0..13 {
        if NUM_4X4_W[b] == w && NUM_4X4_H[b] == hgt {
            return b;
        }
    }
    BLOCK_4X4
}

// ---- leaf kernels (vpx_dsp/loopfilter.c) -------------------------------
// The scalar single-position kernels below are the bit-exactness oracle for the
// SIMD `filter_edge8` (see the `filter_edge8_matches_scalar` test); the shipped
// decoder always goes through `filter_edge8`, so they are test-only.
#[cfg(test)]
mod scalar_ref {
    use super::*;

// High-bit-depth aware leaf math. `base = 1<<(bd-1)` (128 at 8-bit); the signed
// clamp range is `[-base, base-1]`. The mask thresholds are pre-scaled by
// `<<(bd-8)` in `build_lf_info`, and the flat threshold is `1<<(bd-8)`.
#[inline]
fn sclamp(v: i32, base: i32) -> i32 {
    v.clamp(-base, base - 1)
}
#[inline]
fn to_s(p: u16, base: i32) -> i32 {
    p as i32 - base
}
#[inline]
fn from_s(v: i32, base: i32) -> u16 {
    (sclamp(v, base) + base) as u16
}
#[inline]
fn rpo2(x: i32, n: u32) -> u16 {
    ((x + (1 << (n - 1))) >> n) as u16
}

#[inline]
fn filter_mask(limit: i32, blimit: i32, p: [u16; 4], q: [u16; 4]) -> bool {
    let d = |a: u16, b: u16| (a as i32 - b as i32).unsigned_abs();
    let lim = limit as u32;
    d(p[3], p[2]) <= lim
        && d(p[2], p[1]) <= lim
        && d(p[1], p[0]) <= lim
        && d(q[1], q[0]) <= lim
        && d(q[2], q[1]) <= lim
        && d(q[3], q[2]) <= lim
        && d(p[0], q[0]) * 2 + d(p[1], q[1]) / 2 <= blimit as u32
}

#[inline]
fn flat_mask4(p: [u16; 4], q: [u16; 4], flat_thr: u32) -> bool {
    let d = |a: u16, b: u16| (a as i32 - b as i32).unsigned_abs();
    d(p[1], p[0]) <= flat_thr
        && d(q[1], q[0]) <= flat_thr
        && d(p[2], p[0]) <= flat_thr
        && d(q[2], q[0]) <= flat_thr
        && d(p[3], p[0]) <= flat_thr
        && d(q[3], q[0]) <= flat_thr
}

#[inline]
fn hev_mask(thresh: i32, p1: u16, p0: u16, q0: u16, q1: u16) -> bool {
    let d = |a: u16, b: u16| (a as i32 - b as i32).unsigned_abs();
    d(p1, p0) > thresh as u32 || d(q1, q0) > thresh as u32
}

/// 4-tap narrow filter. `buf[i]` is q0; pixels accessed at ±`st`.
fn filter4(buf: &mut [u16], i: usize, st: usize, thresh: i32, base: i32) {
    let p1 = buf[i - 2 * st];
    let p0 = buf[i - st];
    let q0 = buf[i];
    let q1 = buf[i + st];
    let (ps1, ps0, qs0, qs1) = (to_s(p1, base), to_s(p0, base), to_s(q0, base), to_s(q1, base));
    let hev = hev_mask(thresh, p1, p0, q0, q1);
    let mut filter = if hev { sclamp(ps1 - qs1, base) } else { 0 };
    filter = sclamp(filter + 3 * (qs0 - ps0), base);
    let filter1 = sclamp(filter + 4, base) >> 3;
    let filter2 = sclamp(filter + 3, base) >> 3;
    buf[i] = from_s(qs0 - filter1, base);
    buf[i - st] = from_s(ps0 + filter2, base);
    let f = if !hev { (filter1 + 1) >> 1 } else { 0 };
    buf[i + st] = from_s(qs1 - f, base);
    buf[i - 2 * st] = from_s(ps1 + f, base);
}

/// Apply the appropriate-width filter at a single edge position `i` (q0), with
/// across-edge step `st`. `width` is 4, 8, or 16 (mblim is the wide limit).
pub(super) fn filter_edge(buf: &mut [u16], i: usize, st: usize, width: usize, t: &Thresh, bd: i32) {
    let base = 1 << (bd - 1);
    let flat_thr = 1u32 << (bd - 8);
    let g = |buf: &[u16], k: i32| buf[(i as i32 + k * st as i32) as usize];
    let p = [g(buf, -1), g(buf, -2), g(buf, -3), g(buf, -4)];
    let q = [g(buf, 0), g(buf, 1), g(buf, 2), g(buf, 3)];
    if !filter_mask(t.lim, t.mblim, p, q) {
        return;
    }
    let flat = flat_mask4(p, q, flat_thr);
    if width >= 16 && flat {
        // flat2 over the 8-wide neighbourhood.
        let pe = [g(buf, -5), g(buf, -6), g(buf, -7), g(buf, -8)];
        let qe = [g(buf, 4), g(buf, 5), g(buf, 6), g(buf, 7)];
        let d = |a: u16, b: u16| (a as i32 - b as i32).unsigned_abs();
        let flat2 = d(pe[0], p[0]) <= flat_thr
            && d(qe[0], q[0]) <= flat_thr
            && d(pe[1], p[0]) <= flat_thr
            && d(qe[1], q[0]) <= flat_thr
            && d(pe[2], p[0]) <= flat_thr
            && d(qe[2], q[0]) <= flat_thr
            && d(pe[3], p[0]) <= flat_thr
            && d(qe[3], q[0]) <= flat_thr;
        if flat2 {
            filter16(buf, i, st);
            return;
        }
    }
    if width >= 8 && flat {
        filter8(buf, i, st);
    } else {
        filter4(buf, i, st, t.hev_thr, base);
    }
}

/// 7-tap wide filter (used when `flat`).
fn filter8(buf: &mut [u16], i: usize, st: usize) {
    let g = |k: i32| buf[(i as i32 + k * st as i32) as usize] as i32;
    let (p3, p2, p1, p0) = (g(-4), g(-3), g(-2), g(-1));
    let (q0, q1, q2, q3) = (g(0), g(1), g(2), g(3));
    buf[i - 3 * st] = rpo2(p3 + p3 + p3 + 2 * p2 + p1 + p0 + q0, 3);
    buf[i - 2 * st] = rpo2(p3 + p3 + p2 + 2 * p1 + p0 + q0 + q1, 3);
    buf[i - st] = rpo2(p3 + p2 + p1 + 2 * p0 + q0 + q1 + q2, 3);
    buf[i] = rpo2(p2 + p1 + p0 + 2 * q0 + q1 + q2 + q3, 3);
    buf[i + st] = rpo2(p1 + p0 + q0 + 2 * q1 + q2 + q3 + q3, 3);
    buf[i + 2 * st] = rpo2(p0 + q0 + q1 + 2 * q2 + q3 + q3 + q3, 3);
}

/// 15-tap wide filter (used when `flat` and `flat2`).
fn filter16(buf: &mut [u16], i: usize, st: usize) {
    let g = |k: i32| buf[(i as i32 + k * st as i32) as usize] as i32;
    let p: Vec<i32> = (0..8).map(|k| g(-1 - k)).collect(); // p0..p7
    let q: Vec<i32> = (0..8).map(|k| g(k)).collect(); // q0..q7
    let s = |off: i32| (i as i32 + off * st as i32) as usize;
    let (p0, p1, p2, p3, p4, p5, p6, p7) = (p[0], p[1], p[2], p[3], p[4], p[5], p[6], p[7]);
    let (q0, q1, q2, q3, q4, q5, q6, q7) = (q[0], q[1], q[2], q[3], q[4], q[5], q[6], q[7]);
    buf[s(-7)] = rpo2(p7 * 7 + p6 * 2 + p5 + p4 + p3 + p2 + p1 + p0 + q0, 4);
    buf[s(-6)] = rpo2(p7 * 6 + p6 + p5 * 2 + p4 + p3 + p2 + p1 + p0 + q0 + q1, 4);
    buf[s(-5)] = rpo2(p7 * 5 + p6 + p5 + p4 * 2 + p3 + p2 + p1 + p0 + q0 + q1 + q2, 4);
    buf[s(-4)] = rpo2(p7 * 4 + p6 + p5 + p4 + p3 * 2 + p2 + p1 + p0 + q0 + q1 + q2 + q3, 4);
    buf[s(-3)] = rpo2(p7 * 3 + p6 + p5 + p4 + p3 + p2 * 2 + p1 + p0 + q0 + q1 + q2 + q3 + q4, 4);
    buf[s(-2)] = rpo2(p7 * 2 + p6 + p5 + p4 + p3 + p2 + p1 * 2 + p0 + q0 + q1 + q2 + q3 + q4 + q5, 4);
    buf[s(-1)] = rpo2(p7 + p6 + p5 + p4 + p3 + p2 + p1 + p0 * 2 + q0 + q1 + q2 + q3 + q4 + q5 + q6, 4);
    buf[s(0)] = rpo2(p6 + p5 + p4 + p3 + p2 + p1 + p0 + q0 * 2 + q1 + q2 + q3 + q4 + q5 + q6 + q7, 4);
    buf[s(1)] = rpo2(p5 + p4 + p3 + p2 + p1 + p0 + q0 + q1 * 2 + q2 + q3 + q4 + q5 + q6 + q7 * 2, 4);
    buf[s(2)] = rpo2(p4 + p3 + p2 + p1 + p0 + q0 + q1 + q2 * 2 + q3 + q4 + q5 + q6 + q7 * 3, 4);
    buf[s(3)] = rpo2(p3 + p2 + p1 + p0 + q0 + q1 + q2 + q3 * 2 + q4 + q5 + q6 + q7 * 4, 4);
    buf[s(4)] = rpo2(p2 + p1 + p0 + q0 + q1 + q2 + q3 + q4 * 2 + q5 + q6 + q7 * 5, 4);
    buf[s(5)] = rpo2(p1 + p0 + q0 + q1 + q2 + q3 + q4 + q5 * 2 + q6 + q7 * 6, 4);
    buf[s(6)] = rpo2(p0 + q0 + q1 + q2 + q3 + q4 + q5 + q6 * 2 + q7 * 7, 4);
}

} // mod scalar_ref

// ---- per-plane superblock filtering (vp9_filter_block_plane_non420) -----

#[allow(clippy::too_many_arguments)]
fn filter_block_plane(
    buf: &mut [u16],
    stride: usize,
    base_x: usize,
    base_y: usize,
    ss_x: usize,
    ss_y: usize,
    lf: &LfInfo,
    mi_grid: &[ModeInfo],
    mi_rows: usize,
    mi_cols: usize,
    mi_row: usize,
    mi_col: usize,
    bd: i32,
) {
    let row_step = 1 << ss_y;
    let col_step = 1 << ss_x;
    // Masks per mi-row (within the SB), indexed [r].
    let mut m16x16 = [0u32; MI_BLOCK_SIZE];
    let mut m8x8 = [0u32; MI_BLOCK_SIZE];
    let mut m4x4 = [0u32; MI_BLOCK_SIZE];
    let mut m4x4_int = [0u32; MI_BLOCK_SIZE];
    let mut lfl = [0u8; MI_BLOCK_SIZE * MI_BLOCK_SIZE];

    let mut r = 0;
    while r < MI_BLOCK_SIZE && mi_row + r < mi_rows {
        let mut m16c = 0u32;
        let mut m8c = 0u32;
        let mut m4c = 0u32;
        let mut c = 0;
        while c < MI_BLOCK_SIZE && mi_col + c < mi_cols {
            let mi = &mi_grid[(mi_row + r) * mi_cols + (mi_col + c)];
            let sb_type = mi.sb_type as usize;
            let skip_this = mi.skip && mi.is_inter;
            let block_edge_left = if NUM_4X4_W[sb_type] > 1 {
                (c & (NUM_8X8_W[sb_type] - 1)) == 0
            } else {
                true
            };
            let skip_this_c = skip_this && !block_edge_left;
            let block_edge_above = if NUM_4X4_H[sb_type] > 1 {
                (r & (NUM_8X8_H[sb_type] - 1)) == 0
            } else {
                true
            };
            let skip_this_r = skip_this && !block_edge_above;
            let tx_size = plane_tx_size(mi, ss_x, ss_y);
            let skip_border_4x4_c = ss_x == 1 && mi_col + c == mi_cols - 1;
            let skip_border_4x4_r = ss_y == 1 && mi_row + r == mi_rows - 1;
            let cc = c >> ss_x;

            let level = get_filter_level(lf, mi);
            lfl[(r << 3) + cc] = level;
            if level == 0 {
                c += col_step;
                continue;
            }

            if tx_size == 3 {
                if !skip_this_c && (cc & 3) == 0 {
                    if !skip_border_4x4_c { m16c |= 1 << cc } else { m8c |= 1 << cc }
                }
                if !skip_this_r && ((r >> ss_y) & 3) == 0 {
                    if !skip_border_4x4_r { m16x16[r] |= 1 << cc } else { m8x8[r] |= 1 << cc }
                }
            } else if tx_size == 2 {
                if !skip_this_c && (cc & 1) == 0 {
                    if !skip_border_4x4_c { m16c |= 1 << cc } else { m8c |= 1 << cc }
                }
                if !skip_this_r && ((r >> ss_y) & 1) == 0 {
                    if !skip_border_4x4_r { m16x16[r] |= 1 << cc } else { m8x8[r] |= 1 << cc }
                }
            } else {
                if !skip_this_c {
                    if tx_size == 1 || (cc & 3) == 0 { m8c |= 1 << cc } else { m4c |= 1 << cc }
                }
                if !skip_this_r {
                    if tx_size == 1 || ((r >> ss_y) & 3) == 0 { m8x8[r] |= 1 << cc } else { m4x4[r] |= 1 << cc }
                }
                if !skip_this && tx_size == 0 && !skip_border_4x4_c {
                    m4x4_int[r] |= 1 << cc;
                }
            }
            c += col_step;
        }
        // Vertical edges for this mi-row. Disable the frame's leftmost column.
        let border = if mi_col == 0 { !1u32 } else { !0u32 };
        let row_y = base_y + (r >> ss_y) * 8;
        filter_selectively_vert(
            buf,
            stride,
            base_x,
            row_y,
            m16c & border,
            m8c & border,
            m4c & border,
            m4x4_int[r],
            &lf.thr,
            &lfl[r << 3..],
            bd,
        );
        r += row_step;
    }

    // Horizontal pass.
    let mut r = 0;
    while r < MI_BLOCK_SIZE && mi_row + r < mi_rows {
        let skip_border_4x4_r = ss_y == 1 && mi_row + r == mi_rows - 1;
        let m4i = if skip_border_4x4_r { 0 } else { m4x4_int[r] };
        let (mut a16, mut a8, mut a4) = (m16x16[r], m8x8[r], m4x4[r]);
        if mi_row + r == 0 {
            a16 = 0;
            a8 = 0;
            a4 = 0;
        }
        let row_y = base_y + (r >> ss_y) * 8;
        filter_selectively_horiz(buf, stride, base_x, row_y, a16, a8, a4, m4i, &lf.thr, &lfl[r << 3..], bd);
        r += row_step;
    }
}

#[allow(clippy::too_many_arguments)]
fn filter_selectively_vert(
    buf: &mut [u16],
    stride: usize,
    base_x: usize,
    y: usize,
    mut m16: u32,
    mut m8: u32,
    mut m4: u32,
    mut m4i: u32,
    thr: &[Thresh],
    lfl: &[u8],
    bd: i32,
) {
    let mut col = 0;
    let mut any = m16 | m8 | m4 | m4i;
    while any != 0 {
        let t = &thr[lfl[col] as usize];
        let x = base_x + col * 8;
        let base = y * stride + x;
        if m16 & 1 != 0 {
            apply_vert(buf, base, stride, 16, t, bd);
        } else if m8 & 1 != 0 {
            apply_vert(buf, base, stride, 8, t, bd);
        } else if m4 & 1 != 0 {
            apply_vert(buf, base, stride, 4, t, bd);
        }
        if m4i & 1 != 0 {
            apply_vert(buf, base + 4, stride, 4, t, bd);
        }
        col += 1;
        m16 >>= 1;
        m8 >>= 1;
        m4 >>= 1;
        m4i >>= 1;
        any >>= 1;
    }
}

#[allow(clippy::too_many_arguments)]
fn filter_selectively_horiz(
    buf: &mut [u16],
    stride: usize,
    base_x: usize,
    y: usize,
    mut m16: u32,
    mut m8: u32,
    mut m4: u32,
    mut m4i: u32,
    thr: &[Thresh],
    lfl: &[u8],
    bd: i32,
) {
    let mut col = 0;
    let mut any = m16 | m8 | m4 | m4i;
    while any != 0 {
        let t = &thr[lfl[col] as usize];
        let x = base_x + col * 8;
        let base = y * stride + x;
        if m16 & 1 != 0 {
            apply_horiz(buf, base, stride, 16, t, bd);
        } else if m8 & 1 != 0 {
            apply_horiz(buf, base, stride, 8, t, bd);
            if m4i & 1 != 0 {
                apply_horiz(buf, base + 4 * stride, stride, 4, t, bd);
            }
        } else if m4 & 1 != 0 {
            apply_horiz(buf, base, stride, 4, t, bd);
            if m4i & 1 != 0 {
                apply_horiz(buf, base + 4 * stride, stride, 4, t, bd);
            }
        } else if m4i & 1 != 0 {
            apply_horiz(buf, base + 4 * stride, stride, 4, t, bd);
        }
        col += 1;
        m16 >>= 1;
        m8 >>= 1;
        m4 >>= 1;
        m4i >>= 1;
        any >>= 1;
    }
}

/// SIMD-friendly edge filter: applies the width-4/8/16 deblock to **all 8 edge
/// positions at once** in a branchless structure-of-arrays form. Identical
/// integer math to [`filter_edge`] per lane (so bit-exact), but with no
/// data-dependent branches in the 8-wide loops — these autovectorize to AVX2.
///
/// `pos_stride` steps across the edge (the filter direction); `lane_stride`
/// steps between the 8 positions along the edge. The two callers below pick the
/// orientation. `lane_stride == 1` (horizontal edge) makes the gather/scatter
/// contiguous; the strided case (vertical edge) still vectorizes the arithmetic.
#[allow(clippy::needless_range_loop)]
fn filter_edge8(buf: &mut [u16], i: usize, pos_stride: usize, lane_stride: usize, width: usize, t: &Thresh, bd: i32) {
    // AVX2: horizontal edges (lane_stride == 1) gather/scatter contiguously;
    // vertical edges transpose an 8×8 (or 8×16) tile in-register.
    #[cfg(target_arch = "x86_64")]
    if std::is_x86_feature_detected!("avx2") {
        // SAFETY: AVX2 confirmed; the caller's mask guarantees the p7..q7 read
        // window (along `pos_stride`, 8 lanes along `lane_stride`) is in-bounds.
        unsafe { filter_edge8_avx2(buf, i, pos_stride, lane_stride, width, t, bd) };
        return;
    }
    let base = 1i32 << (bd - 1);
    let ft = 1i32 << (bd - 8); // flat threshold
    let (lim, blimit, hev_thr) = (t.lim, t.mblim, t.hev_thr);

    // Gather p0..p7 (toward -pos_stride) and q0..q7 (toward +pos_stride), one
    // i32 lane per edge position. The number of `pe` taps needed depends on the
    // filter width; load all 8 only for width 16.
    let outer = if width >= 16 { 8 } else { 4 };
    let mut p = [[0i32; 8]; 8];
    let mut q = [[0i32; 8]; 8];
    for l in 0..8 {
        let c = i + l * lane_stride;
        for k in 0..outer {
            p[k][l] = buf[c - (k + 1) * pos_stride] as i32;
            q[k][l] = buf[c + k * pos_stride] as i32;
        }
    }

    let ad = |a: [i32; 8], b: [i32; 8]| -> [i32; 8] { core::array::from_fn(|l| (a[l] - b[l]).abs()) };
    let le = |d: [i32; 8], t: i32| -> [i32; 8] { core::array::from_fn(|l| -((d[l] <= t) as i32)) };
    let gt = |d: [i32; 8], t: i32| -> [i32; 8] { core::array::from_fn(|l| -((d[l] > t) as i32)) };
    let and = |a: [i32; 8], b: [i32; 8]| -> [i32; 8] { core::array::from_fn(|l| a[l] & b[l]) };
    let andn = |a: [i32; 8], m: [i32; 8]| -> [i32; 8] { core::array::from_fn(|l| a[l] & !m[l]) };
    // select: m ? a : b   (m is 0 or -1 per lane)
    let sel = |m: [i32; 8], a: [i32; 8], b: [i32; 8]| -> [i32; 8] {
        core::array::from_fn(|l| (a[l] & m[l]) | (b[l] & !m[l]))
    };

    // filter_mask (8.8.1): all neighbour deltas within `lim`, and the cross-edge
    // gradient within `blimit`.
    let mut mask = and(le(ad(p[3], p[2]), lim), le(ad(p[2], p[1]), lim));
    mask = and(mask, le(ad(p[1], p[0]), lim));
    mask = and(mask, le(ad(q[1], q[0]), lim));
    mask = and(mask, le(ad(q[2], q[1]), lim));
    mask = and(mask, le(ad(q[3], q[2]), lim));
    let grad: [i32; 8] = core::array::from_fn(|l| (p[0][l] - q[0][l]).abs() * 2 + (p[1][l] - q[1][l]).abs() / 2);
    mask = and(mask, le(grad, blimit));

    // flat (flat_mask4) over the inner 4 taps.
    let mut flat = and(le(ad(p[1], p[0]), ft), le(ad(q[1], q[0]), ft));
    flat = and(flat, le(ad(p[2], p[0]), ft));
    flat = and(flat, le(ad(q[2], q[0]), ft));
    flat = and(flat, le(ad(p[3], p[0]), ft));
    flat = and(flat, le(ad(q[3], q[0]), ft));

    // hev (high edge variance).
    let hev: [i32; 8] = core::array::from_fn(|l| gt(ad(p[1], p[0]), hev_thr)[l] | gt(ad(q[1], q[0]), hev_thr)[l]);

    // --- filter4 (always computed; selected where !flat) ---
    let scl = |v: [i32; 8]| -> [i32; 8] { core::array::from_fn(|l| v[l].clamp(-base, base - 1)) };
    let ps1: [i32; 8] = core::array::from_fn(|l| p[1][l] - base);
    let ps0: [i32; 8] = core::array::from_fn(|l| p[0][l] - base);
    let qs0: [i32; 8] = core::array::from_fn(|l| q[0][l] - base);
    let qs1: [i32; 8] = core::array::from_fn(|l| q[1][l] - base);
    let f_hev = scl(core::array::from_fn(|l| ps1[l] - qs1[l]));
    let mut filt: [i32; 8] = core::array::from_fn(|l| f_hev[l] & hev[l]); // hev ? f_hev : 0
    filt = scl(core::array::from_fn(|l| filt[l] + 3 * (qs0[l] - ps0[l])));
    let filter1: [i32; 8] = core::array::from_fn(|l| scl(core::array::from_fn(|j| filt[j] + 4))[l] >> 3);
    let filter2: [i32; 8] = core::array::from_fn(|l| scl(core::array::from_fn(|j| filt[j] + 3))[l] >> 3);
    let f4q0: [i32; 8] = core::array::from_fn(|l| scl(core::array::from_fn(|j| qs0[j] - filter1[j]))[l] + base);
    let f4p0: [i32; 8] = core::array::from_fn(|l| scl(core::array::from_fn(|j| ps0[j] + filter2[j]))[l] + base);
    let ff: [i32; 8] = core::array::from_fn(|l| ((filter1[l] + 1) >> 1) & !hev[l]); // !hev ? (f1+1)>>1 : 0
    let f4q1: [i32; 8] = core::array::from_fn(|l| scl(core::array::from_fn(|j| qs1[j] - ff[j]))[l] + base);
    let f4p1: [i32; 8] = core::array::from_fn(|l| scl(core::array::from_fn(|j| ps1[j] + ff[j]))[l] + base);

    let r3 = |x: [i32; 8]| -> [i32; 8] { core::array::from_fn(|l| (x[l] + 4) >> 3) }; // round_pow2(.,3)

    if width < 8 {
        // width 4: filter4 where mask.
        let np0 = sel(mask, f4p0, p[0]);
        let np1 = sel(mask, f4p1, p[1]);
        let nq0 = sel(mask, f4q0, q[0]);
        let nq1 = sel(mask, f4q1, q[1]);
        scatter(buf, i, pos_stride, lane_stride, &[(0, np0), (1, np1)], &[(0, nq0), (1, nq1)]);
        return;
    }

    // --- filter8 (7-tap, computed for width>=8; selected where flat) ---
    let (p0, p1, p2, p3) = (p[0], p[1], p[2], p[3]);
    let (q0, q1, q2, q3) = (q[0], q[1], q[2], q[3]);
    let f8p2 = r3(core::array::from_fn(|l| p3[l] * 3 + 2 * p2[l] + p1[l] + p0[l] + q0[l]));
    let f8p1 = r3(core::array::from_fn(|l| p3[l] + p3[l] + p2[l] + 2 * p1[l] + p0[l] + q0[l] + q1[l]));
    let f8p0 = r3(core::array::from_fn(|l| p3[l] + p2[l] + p1[l] + 2 * p0[l] + q0[l] + q1[l] + q2[l]));
    let f8q0 = r3(core::array::from_fn(|l| p2[l] + p1[l] + p0[l] + 2 * q0[l] + q1[l] + q2[l] + q3[l]));
    let f8q1 = r3(core::array::from_fn(|l| p1[l] + p0[l] + q0[l] + 2 * q1[l] + q2[l] + q3[l] + q3[l]));
    let f8q2 = r3(core::array::from_fn(|l| p0[l] + q0[l] + q1[l] + 2 * q2[l] + q3[l] + q3[l] + q3[l]));

    if width < 16 {
        let use8 = and(mask, flat);
        let use4 = andn(mask, flat);
        let np2 = sel(use8, f8p2, p2);
        let np1 = sel(use8, f8p1, sel(use4, f4p1, p1));
        let np0 = sel(use8, f8p0, sel(use4, f4p0, p0));
        let nq0 = sel(use8, f8q0, sel(use4, f4q0, q0));
        let nq1 = sel(use8, f8q1, sel(use4, f4q1, q1));
        let nq2 = sel(use8, f8q2, q2);
        scatter(buf, i, pos_stride, lane_stride,
            &[(0, np0), (1, np1), (2, np2)], &[(0, nq0), (1, nq1), (2, nq2)]);
        return;
    }

    // --- flat2 + filter16 (15-tap), width 16 ---
    let (p4, p5, p6, p7) = (p[4], p[5], p[6], p[7]);
    let (q4, q5, q6, q7) = (q[4], q[5], q[6], q[7]);
    let mut flat2 = and(le(ad(p4, p0), ft), le(ad(q4, q0), ft));
    flat2 = and(flat2, le(ad(p5, p0), ft));
    flat2 = and(flat2, le(ad(q5, q0), ft));
    flat2 = and(flat2, le(ad(p6, p0), ft));
    flat2 = and(flat2, le(ad(q6, q0), ft));
    flat2 = and(flat2, le(ad(p7, p0), ft));
    flat2 = and(flat2, le(ad(q7, q0), ft));
    let r4 = |x: [i32; 8]| -> [i32; 8] { core::array::from_fn(|l| (x[l] + 8) >> 4) };
    let s = |arr: &[[i32; 8]]| -> [i32; 8] { core::array::from_fn(|l| arr.iter().map(|a| a[l]).sum()) };
    let f16p6 = r4(s(&[mul(p7, 7), mul(p6, 2), p5, p4, p3, p2, p1, p0, q0]));
    let f16p5 = r4(s(&[mul(p7, 6), p6, mul(p5, 2), p4, p3, p2, p1, p0, q0, q1]));
    let f16p4 = r4(s(&[mul(p7, 5), p6, p5, mul(p4, 2), p3, p2, p1, p0, q0, q1, q2]));
    let f16p3 = r4(s(&[mul(p7, 4), p6, p5, p4, mul(p3, 2), p2, p1, p0, q0, q1, q2, q3]));
    let f16p2 = r4(s(&[mul(p7, 3), p6, p5, p4, p3, mul(p2, 2), p1, p0, q0, q1, q2, q3, q4]));
    let f16p1 = r4(s(&[mul(p7, 2), p6, p5, p4, p3, p2, mul(p1, 2), p0, q0, q1, q2, q3, q4, q5]));
    let f16p0 = r4(s(&[p7, p6, p5, p4, p3, p2, p1, mul(p0, 2), q0, q1, q2, q3, q4, q5, q6]));
    let f16q0 = r4(s(&[p6, p5, p4, p3, p2, p1, p0, mul(q0, 2), q1, q2, q3, q4, q5, q6, q7]));
    let f16q1 = r4(s(&[p5, p4, p3, p2, p1, p0, q0, mul(q1, 2), q2, q3, q4, q5, q6, mul(q7, 2)]));
    let f16q2 = r4(s(&[p4, p3, p2, p1, p0, q0, q1, mul(q2, 2), q3, q4, q5, q6, mul(q7, 3)]));
    let f16q3 = r4(s(&[p3, p2, p1, p0, q0, q1, q2, mul(q3, 2), q4, q5, q6, mul(q7, 4)]));
    let f16q4 = r4(s(&[p2, p1, p0, q0, q1, q2, q3, mul(q4, 2), q5, q6, mul(q7, 5)]));
    let f16q5 = r4(s(&[p1, p0, q0, q1, q2, q3, q4, mul(q5, 2), q6, mul(q7, 6)]));
    let f16q6 = r4(s(&[p0, q0, q1, q2, q3, q4, q5, mul(q6, 2), mul(q7, 7)]));

    let use16 = and(and(mask, flat), flat2); // mask & flat & flat2
    let notflat2: [i32; 8] = core::array::from_fn(|l| !flat2[l]);
    let use8 = and(and(mask, flat), notflat2); // mask & flat & !flat2
    let use4 = andn(mask, flat); // mask & !flat

    let np6 = sel(use16, f16p6, p6);
    let np5 = sel(use16, f16p5, p5);
    let np4 = sel(use16, f16p4, p4);
    let np3 = sel(use16, f16p3, p3);
    let np2 = sel(use16, f16p2, sel(use8, f8p2, p2));
    let np1 = sel(use16, f16p1, sel(use8, f8p1, sel(use4, f4p1, p1)));
    let np0 = sel(use16, f16p0, sel(use8, f8p0, sel(use4, f4p0, p0)));
    let nq0 = sel(use16, f16q0, sel(use8, f8q0, sel(use4, f4q0, q0)));
    let nq1 = sel(use16, f16q1, sel(use8, f8q1, sel(use4, f4q1, q1)));
    let nq2 = sel(use16, f16q2, sel(use8, f8q2, q2));
    let nq3 = sel(use16, f16q3, q3);
    let nq4 = sel(use16, f16q4, q4);
    let nq5 = sel(use16, f16q5, q5);
    let nq6 = sel(use16, f16q6, q6);
    scatter(buf, i, pos_stride, lane_stride,
        &[(0, np0), (1, np1), (2, np2), (3, np3), (4, np4), (5, np5), (6, np6)],
        &[(0, nq0), (1, nq1), (2, nq2), (3, nq3), (4, nq4), (5, nq5), (6, nq6)]);
}

#[inline]
fn mul(a: [i32; 8], k: i32) -> [i32; 8] {
    core::array::from_fn(|l| a[l] * k)
}

/// Scatter modified p/q lanes back: `pk` entries write `buf[i + l*lane_stride - (k+1)*pos_stride]`,
/// `qk` entries write `buf[i + l*lane_stride + k*pos_stride]`.
#[inline]
fn scatter(buf: &mut [u16], i: usize, pos_stride: usize, lane_stride: usize, pk: &[(usize, [i32; 8])], qk: &[(usize, [i32; 8])]) {
    for l in 0..8 {
        let c = i + l * lane_stride;
        for &(k, v) in pk {
            buf[c - (k + 1) * pos_stride] = v[l] as u16;
        }
        for &(k, v) in qk {
            buf[c + k * pos_stride] = v[l] as u16;
        }
    }
}

/// In-register transpose of an 8×8 `i32` tile (8 `__m256i`, lane = column → lane
/// = row). Standard unpack/permute network; pure data movement, value-preserving.
#[cfg(target_arch = "x86_64")]
#[target_feature(enable = "avx2")]
unsafe fn transpose8(r: &mut [std::arch::x86_64::__m256i; 8]) {
    use std::arch::x86_64::*;
    let a0 = _mm256_unpacklo_epi32(r[0], r[1]);
    let a1 = _mm256_unpackhi_epi32(r[0], r[1]);
    let a2 = _mm256_unpacklo_epi32(r[2], r[3]);
    let a3 = _mm256_unpackhi_epi32(r[2], r[3]);
    let a4 = _mm256_unpacklo_epi32(r[4], r[5]);
    let a5 = _mm256_unpackhi_epi32(r[4], r[5]);
    let a6 = _mm256_unpacklo_epi32(r[6], r[7]);
    let a7 = _mm256_unpackhi_epi32(r[6], r[7]);
    let b0 = _mm256_unpacklo_epi64(a0, a2);
    let b1 = _mm256_unpackhi_epi64(a0, a2);
    let b2 = _mm256_unpacklo_epi64(a1, a3);
    let b3 = _mm256_unpackhi_epi64(a1, a3);
    let b4 = _mm256_unpacklo_epi64(a4, a6);
    let b5 = _mm256_unpackhi_epi64(a4, a6);
    let b6 = _mm256_unpacklo_epi64(a5, a7);
    let b7 = _mm256_unpackhi_epi64(a5, a7);
    r[0] = _mm256_permute2x128_si256::<0x20>(b0, b4);
    r[1] = _mm256_permute2x128_si256::<0x20>(b1, b5);
    r[2] = _mm256_permute2x128_si256::<0x20>(b2, b6);
    r[3] = _mm256_permute2x128_si256::<0x20>(b3, b7);
    r[4] = _mm256_permute2x128_si256::<0x31>(b0, b4);
    r[5] = _mm256_permute2x128_si256::<0x31>(b1, b5);
    r[6] = _mm256_permute2x128_si256::<0x31>(b2, b6);
    r[7] = _mm256_permute2x128_si256::<0x31>(b3, b7);
}

/// The 8-wide deblock filter math (filter4/8/16 + masks + width-based blend),
/// bit-identical to the scalar reference. `p[k]`/`q[k]` are i32x8 vectors (one
/// edge position each, lane = an independent edge sample). Returns the new
/// `p0..p6` and `q0..q6`; unmodified positions equal their input.
#[cfg(target_arch = "x86_64")]
#[target_feature(enable = "avx2")]
unsafe fn lf_core8(
    p: &[std::arch::x86_64::__m256i; 8],
    q: &[std::arch::x86_64::__m256i; 8],
    width: usize,
    t: &Thresh,
    bd: i32,
) -> ([std::arch::x86_64::__m256i; 7], [std::arch::x86_64::__m256i; 7]) {
    use std::arch::x86_64::*;
    let basei = 1i32 << (bd - 1);
    let ftv = _mm256_set1_epi32(1i32 << (bd - 8));
    let one = _mm256_set1_epi32(1);
    let limv = _mm256_set1_epi32(t.lim);
    let blv = _mm256_set1_epi32(t.mblim);
    let hevv = _mm256_set1_epi32(t.hev_thr);
    let base = _mm256_set1_epi32(basei);
    let nbase = _mm256_set1_epi32(-basei);
    let bm1 = _mm256_set1_epi32(basei - 1);
    let ad = |a, b| _mm256_abs_epi32(_mm256_sub_epi32(a, b));
    let le = |d, tv| _mm256_cmpgt_epi32(_mm256_add_epi32(tv, one), d);
    let gt = |d, tv| _mm256_cmpgt_epi32(d, tv);
    let and = |a, b| _mm256_and_si256(a, b);
    let sel = |m, a, b| _mm256_or_si256(_mm256_and_si256(m, a), _mm256_andnot_si256(m, b));
    let scl = |v| _mm256_max_epi32(_mm256_min_epi32(v, bm1), nbase);
    let add = |a, b| _mm256_add_epi32(a, b);
    let x2 = |a| _mm256_slli_epi32::<1>(a);
    let (p0, p1, p2, p3) = (p[0], p[1], p[2], p[3]);
    let (q0, q1, q2, q3) = (q[0], q[1], q[2], q[3]);

    let mut mask = and(le(ad(p3, p2), limv), le(ad(p2, p1), limv));
    mask = and(mask, le(ad(p1, p0), limv));
    mask = and(mask, le(ad(q1, q0), limv));
    mask = and(mask, le(ad(q2, q1), limv));
    mask = and(mask, le(ad(q3, q2), limv));
    let grad = add(x2(ad(p0, q0)), _mm256_srli_epi32::<1>(ad(p1, q1)));
    mask = and(mask, le(grad, blv));

    let mut flat = and(le(ad(p1, p0), ftv), le(ad(q1, q0), ftv));
    flat = and(flat, le(ad(p2, p0), ftv));
    flat = and(flat, le(ad(q2, q0), ftv));
    flat = and(flat, le(ad(p3, p0), ftv));
    flat = and(flat, le(ad(q3, q0), ftv));

    let hev = _mm256_or_si256(gt(ad(p1, p0), hevv), gt(ad(q1, q0), hevv));

    let s = |v| _mm256_sub_epi32(v, base);
    let (ps1, ps0, qs0, qs1) = (s(p1), s(p0), s(q0), s(q1));
    let f_hev = scl(_mm256_sub_epi32(ps1, qs1));
    let mut filt = _mm256_and_si256(f_hev, hev);
    filt = scl(add(filt, _mm256_mullo_epi32(_mm256_set1_epi32(3), _mm256_sub_epi32(qs0, ps0))));
    let filter1 = _mm256_srai_epi32::<3>(scl(add(filt, _mm256_set1_epi32(4))));
    let filter2 = _mm256_srai_epi32::<3>(scl(add(filt, _mm256_set1_epi32(3))));
    let frm = |v| add(scl(v), base);
    let f4q0 = frm(_mm256_sub_epi32(qs0, filter1));
    let f4p0 = frm(add(ps0, filter2));
    let ff = _mm256_andnot_si256(hev, _mm256_srai_epi32::<1>(add(filter1, one)));
    let f4q1 = frm(_mm256_sub_epi32(qs1, ff));
    let f4p1 = frm(add(ps1, ff));

    let mut np = *p_to_arr(p); // start unmodified
    let mut nq = *p_to_arr(q);
    if width < 8 {
        np[0] = sel(mask, f4p0, p0);
        np[1] = sel(mask, f4p1, p1);
        nq[0] = sel(mask, f4q0, q0);
        nq[1] = sel(mask, f4q1, q1);
        return (np, nq);
    }

    let r3 = |x| _mm256_srai_epi32::<3>(add(x, _mm256_set1_epi32(4)));
    let f8p2 = r3(add(add(add(_mm256_mullo_epi32(_mm256_set1_epi32(3), p3), x2(p2)), add(p1, p0)), q0));
    let f8p1 = r3(add(add(add(p3, p3), add(p2, x2(p1))), add(add(p0, q0), q1)));
    let f8p0 = r3(add(add(add(p3, p2), add(p1, x2(p0))), add(add(q0, q1), q2)));
    let f8q0 = r3(add(add(add(p2, p1), add(p0, x2(q0))), add(add(q1, q2), q3)));
    let f8q1 = r3(add(add(add(p1, p0), add(q0, x2(q1))), add(add(q2, q3), q3)));
    let f8q2 = r3(add(add(add(p0, q0), add(q1, x2(q2))), add(add(q3, q3), q3)));

    if width < 16 {
        let use8 = and(mask, flat);
        let use4 = _mm256_andnot_si256(flat, mask);
        np[2] = sel(use8, f8p2, p2);
        np[1] = sel(use8, f8p1, sel(use4, f4p1, p1));
        np[0] = sel(use8, f8p0, sel(use4, f4p0, p0));
        nq[0] = sel(use8, f8q0, sel(use4, f4q0, q0));
        nq[1] = sel(use8, f8q1, sel(use4, f4q1, q1));
        nq[2] = sel(use8, f8q2, q2);
        return (np, nq);
    }

    let (p4, p5, p6, p7) = (p[4], p[5], p[6], p[7]);
    let (q4, q5, q6, q7) = (q[4], q[5], q[6], q[7]);
    let mut flat2 = and(le(ad(p4, p0), ftv), le(ad(q4, q0), ftv));
    flat2 = and(flat2, le(ad(p5, p0), ftv));
    flat2 = and(flat2, le(ad(q5, q0), ftv));
    flat2 = and(flat2, le(ad(p6, p0), ftv));
    flat2 = and(flat2, le(ad(q6, q0), ftv));
    flat2 = and(flat2, le(ad(p7, p0), ftv));
    flat2 = and(flat2, le(ad(q7, q0), ftv));
    let r4 = |x| _mm256_srai_epi32::<4>(add(x, _mm256_set1_epi32(8)));
    let mk = |k, a| _mm256_mullo_epi32(_mm256_set1_epi32(k), a);
    let sum = |xs: &[__m256i]| xs.iter().copied().reduce(|a, b| _mm256_add_epi32(a, b)).unwrap();
    let f16p6 = r4(sum(&[mk(7, p7), x2(p6), p5, p4, p3, p2, p1, p0, q0]));
    let f16p5 = r4(sum(&[mk(6, p7), p6, x2(p5), p4, p3, p2, p1, p0, q0, q1]));
    let f16p4 = r4(sum(&[mk(5, p7), p6, p5, x2(p4), p3, p2, p1, p0, q0, q1, q2]));
    let f16p3 = r4(sum(&[mk(4, p7), p6, p5, p4, x2(p3), p2, p1, p0, q0, q1, q2, q3]));
    let f16p2 = r4(sum(&[mk(3, p7), p6, p5, p4, p3, x2(p2), p1, p0, q0, q1, q2, q3, q4]));
    let f16p1 = r4(sum(&[x2(p7), p6, p5, p4, p3, p2, x2(p1), p0, q0, q1, q2, q3, q4, q5]));
    let f16p0 = r4(sum(&[p7, p6, p5, p4, p3, p2, p1, x2(p0), q0, q1, q2, q3, q4, q5, q6]));
    let f16q0 = r4(sum(&[p6, p5, p4, p3, p2, p1, p0, x2(q0), q1, q2, q3, q4, q5, q6, q7]));
    let f16q1 = r4(sum(&[p5, p4, p3, p2, p1, p0, q0, x2(q1), q2, q3, q4, q5, q6, x2(q7)]));
    let f16q2 = r4(sum(&[p4, p3, p2, p1, p0, q0, q1, x2(q2), q3, q4, q5, q6, mk(3, q7)]));
    let f16q3 = r4(sum(&[p3, p2, p1, p0, q0, q1, q2, x2(q3), q4, q5, q6, mk(4, q7)]));
    let f16q4 = r4(sum(&[p2, p1, p0, q0, q1, q2, q3, x2(q4), q5, q6, mk(5, q7)]));
    let f16q5 = r4(sum(&[p1, p0, q0, q1, q2, q3, q4, x2(q5), q6, mk(6, q7)]));
    let f16q6 = r4(sum(&[p0, q0, q1, q2, q3, q4, q5, x2(q6), mk(7, q7)]));

    let use16 = and(and(mask, flat), flat2);
    let use8 = and(and(mask, flat), _mm256_andnot_si256(flat2, _mm256_set1_epi32(-1)));
    let use4 = _mm256_andnot_si256(flat, mask);
    np[6] = sel(use16, f16p6, p6);
    np[5] = sel(use16, f16p5, p5);
    np[4] = sel(use16, f16p4, p4);
    np[3] = sel(use16, f16p3, p3);
    np[2] = sel(use16, f16p2, sel(use8, f8p2, p2));
    np[1] = sel(use16, f16p1, sel(use8, f8p1, sel(use4, f4p1, p1)));
    np[0] = sel(use16, f16p0, sel(use8, f8p0, sel(use4, f4p0, p0)));
    nq[0] = sel(use16, f16q0, sel(use8, f8q0, sel(use4, f4q0, q0)));
    nq[1] = sel(use16, f16q1, sel(use8, f8q1, sel(use4, f4q1, q1)));
    nq[2] = sel(use16, f16q2, sel(use8, f8q2, q2));
    nq[3] = sel(use16, f16q3, q3);
    nq[4] = sel(use16, f16q4, q4);
    nq[5] = sel(use16, f16q5, q5);
    nq[6] = sel(use16, f16q6, q6);
    (np, nq)
}

#[cfg(target_arch = "x86_64")]
#[inline]
fn p_to_arr(p: &[std::arch::x86_64::__m256i; 8]) -> &[std::arch::x86_64::__m256i; 7] {
    // first 7 of 8; safe reinterpret of the leading array prefix
    unsafe { &*(p.as_ptr() as *const [std::arch::x86_64::__m256i; 7]) }
}

/// AVX2 deblock for one edge (8 samples). Horizontal edges (`lane_stride == 1`)
/// load/store contiguously; vertical edges transpose an 8×8 / 8×16 tile so the
/// shared [`lf_core8`] always sees lane = edge-sample. Bit-identical to scalar.
///
/// # Safety
/// AVX2 present; the p7..q7 window in-bounds (caller's mask guarantees it).
#[cfg(target_arch = "x86_64")]
#[target_feature(enable = "avx2")]
unsafe fn filter_edge8_avx2(buf: &mut [u16], i: usize, pos_stride: usize, lane_stride: usize, width: usize, t: &Thresh, bd: i32) {
    use std::arch::x86_64::*;
    let bp = buf.as_mut_ptr();
    let ls = lane_stride as isize;
    let ps = pos_stride as isize;
    let n = if width >= 16 { 8usize } else { 4 }; // taps each side
    let zero = _mm256_setzero_si256();

    // Gather the 2n edge positions into i32x8 vectors, lane = edge-sample.
    let mut p = [zero; 8];
    let mut q = [zero; 8];
    if lane_stride == 1 {
        // contiguous lanes: one load per position.
        let ld = |r: isize| _mm256_cvtepu16_epi32(_mm_loadu_si128(bp.offset(i as isize + r * ps) as *const __m128i));
        for k in 0..n {
            p[k] = ld(-(k as isize) - 1);
            q[k] = ld(k as isize);
        }
    } else if n == 4 {
        // vertical, narrow: one 8-col tile [i-4 .. i+4) = [p3,p2,p1,p0,q0,q1,q2,q3].
        let mut col = [zero; 8];
        for l in 0..8 {
            let row = i as isize + l as isize * ls;
            col[l] = _mm256_cvtepu16_epi32(_mm_loadu_si128(bp.offset(row - 4) as *const __m128i));
        }
        transpose8(&mut col); // col[j] = column (i-4+j) across rows
        p[0] = col[3]; p[1] = col[2]; p[2] = col[1]; p[3] = col[0];
        q[0] = col[4]; q[1] = col[5]; q[2] = col[6]; q[3] = col[7];
    } else {
        // vertical, wide: two 8-col tiles, p7..p0 and q0..q7.
        let mut lo = [zero; 8];
        let mut hi = [zero; 8];
        for l in 0..8 {
            let row = i as isize + l as isize * ls;
            lo[l] = _mm256_cvtepu16_epi32(_mm_loadu_si128(bp.offset(row - 8) as *const __m128i));
            hi[l] = _mm256_cvtepu16_epi32(_mm_loadu_si128(bp.offset(row) as *const __m128i));
        }
        transpose8(&mut lo); // lo[j] = column (i-8+j) = p(7-j)
        transpose8(&mut hi); // hi[j] = column (i+j) = q(j)
        for k in 0..8 { p[k] = lo[7 - k]; q[k] = hi[k]; }
    }

    let (np, nq) = lf_core8(&p, &q, width, t, bd);

    // number of modified positions each side: 2 / 3 / 7.
    let m = if width >= 16 { 7 } else if width >= 8 { 3 } else { 2 };

    if lane_stride == 1 {
        let st = |r: isize, v: __m256i| {
            let packed = _mm256_permute4x64_epi64::<0x08>(_mm256_packus_epi32(v, v));
            _mm_storeu_si128(bp.offset(i as isize + r * ps) as *mut __m128i, _mm256_castsi256_si128(packed));
        };
        for k in 0..m { st(-(k as isize) - 1, np[k]); st(k as isize, nq[k]); }
    } else if n == 4 {
        // rebuild the 8-col tile [p3,p2,p1,p0,q0,q1,q2,q3] with modified values.
        let mut col = [zero; 8];
        col[0] = p[3]; col[1] = p[2]; col[2] = p[1]; col[3] = p[0];
        col[4] = q[0]; col[5] = q[1]; col[6] = q[2]; col[7] = q[3];
        for k in 0..m { col[3 - k] = np[k]; col[4 + k] = nq[k]; }
        transpose8(&mut col);
        for l in 0..8 {
            let row = i as isize + l as isize * ls;
            let packed = _mm256_permute4x64_epi64::<0x08>(_mm256_packus_epi32(col[l], col[l]));
            _mm_storeu_si128(bp.offset(row - 4) as *mut __m128i, _mm256_castsi256_si128(packed));
        }
    } else {
        // width 16 vertical: rebuild both 8-col tiles, transpose, store.
        let mut lo = [zero; 8]; // p7..p0
        let mut hi = [zero; 8]; // q0..q7
        for k in 0..8 { lo[7 - k] = p[k]; hi[k] = q[k]; }
        for k in 0..m { lo[7 - k] = np[k]; hi[k] = nq[k]; }
        transpose8(&mut lo);
        transpose8(&mut hi);
        for l in 0..8 {
            let row = i as isize + l as isize * ls;
            let plo = _mm256_permute4x64_epi64::<0x08>(_mm256_packus_epi32(lo[l], lo[l]));
            let phi = _mm256_permute4x64_epi64::<0x08>(_mm256_packus_epi32(hi[l], hi[l]));
            _mm_storeu_si128(bp.offset(row - 8) as *mut __m128i, _mm256_castsi256_si128(plo));
            _mm_storeu_si128(bp.offset(row) as *mut __m128i, _mm256_castsi256_si128(phi));
        }
    }
}

/// Vertical edge (filter across columns): 8 (or 16) pixels down the edge.
fn apply_vert(buf: &mut [u16], base: usize, stride: usize, width: usize, t: &Thresh, bd: i32) {
    filter_edge8(buf, base, 1, stride, width, t, bd);
}

/// Horizontal edge (filter across rows): 8 pixels along the edge.
fn apply_horiz(buf: &mut [u16], base: usize, stride: usize, width: usize, t: &Thresh, bd: i32) {
    filter_edge8(buf, base, stride, 1, width, t, bd);
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The branchless 8-wide [`filter_edge8`] must be bit-identical to applying
    /// the scalar [`filter_edge`] at each of the 8 edge positions, for every
    /// width, both orientations, and across flat / textured / step content that
    /// exercises filter4/8/16 selection and the filter-mask rejection path.
    #[test]
    fn filter_edge8_matches_scalar() {
        use super::scalar_ref::filter_edge;
        let mut s: u32 = 0x9e3779b9;
        let mut rng = || {
            s ^= s << 13;
            s ^= s >> 17;
            s ^= s << 5;
            s
        };
        let stride = 40usize;
        let bd = 8;
        for &(lim, mblim, hev) in &[(8i32, 28, 1i32), (4, 16, 0), (16, 52, 2), (1, 9, 0)] {
            let t = Thresh { lim, mblim, hev_thr: hev };
            for &width in &[4usize, 8, 16] {
                for kind in 0..3 {
                    let mut buf = vec![0u16; stride * 40];
                    for (idx, v) in buf.iter_mut().enumerate() {
                        *v = match kind {
                            0 => (rng() % 256) as u16,                       // textured
                            1 => (110 + (rng() % 5)) as u16,                 // near-flat
                            _ => if (idx % stride) >= 20 { 200 } else { 40 }, // step edge
                        };
                    }
                    let base = 16 * stride + 16;
                    // horizontal edge
                    let (mut a, mut b) = (buf.clone(), buf.clone());
                    for k in 0..8 {
                        filter_edge(&mut a, base + k, stride, width, &t, bd);
                    }
                    filter_edge8(&mut b, base, stride, 1, width, &t, bd);
                    assert_eq!(a, b, "horiz w={width} kind={kind} lim={lim}");
                    // vertical edge
                    let (mut a, mut b) = (buf.clone(), buf.clone());
                    for k in 0..8 {
                        filter_edge(&mut a, base + k * stride, 1, width, &t, bd);
                    }
                    filter_edge8(&mut b, base, 1, stride, width, &t, bd);
                    assert_eq!(a, b, "vert w={width} kind={kind} lim={lim}");
                }
            }
        }
    }

    fn hdr(level: u32, sharpness: u32) -> FrameHeader {
        let mut h = FrameHeader::default();
        h.loop_filter_level = level;
        h.loop_filter_sharpness = sharpness;
        h.lf_delta_enabled = false;
        h
    }

    #[test]
    fn thresholds_match_libvpx() {
        // sharpness 0: lim == level, mblim == 2*(level+2)+lim, hev == level>>4.
        let lf = build_lf_info(&hdr(20, 0));
        assert_eq!(lf.thr[4].lim, 4);
        assert_eq!(lf.thr[4].mblim, 2 * (4 + 2) + 4);
        assert_eq!(lf.thr[63].mblim, 2 * 65 + 63);
        assert_eq!(lf.thr[63].hev_thr, 3);
        // sharpness clamps the inside limit to (9 - sharpness) and floor 1.
        let lf = build_lf_info(&hdr(40, 5));
        // bil = 40 >> (1+1) = 10, clamped to 9-5 = 4.
        assert_eq!(lf.thr[40].lim, 4);
        // level 0 -> lim floored to 1.
        assert_eq!(lf.thr[0].lim, 1);
    }

    #[test]
    fn level_zero_is_noop() {
        // A zero level disables filtering entirely (handled in loop_filter_frame).
        let lf = build_lf_info(&hdr(0, 0));
        assert_eq!(lf.lvl[0][0][0], 0);
    }
}

/// Deblock a fully-reconstructed frame in place. `planes` are (buf, stride) per
/// plane with their subsampling; `mi_grid` is the row-major mode-info grid.
#[allow(clippy::too_many_arguments)]
pub fn loop_filter_frame(
    planes: &mut [(&mut [u16], usize, usize, usize)], // (buf, stride, ss_x, ss_y)
    mi_grid: &[ModeInfo],
    mi_rows: usize,
    mi_cols: usize,
    h: &FrameHeader,
) {
    if h.loop_filter_level == 0 {
        return;
    }
    let lf = build_lf_info(h);
    let bd = h.bit_depth.max(8) as i32;
    let mut mi_row = 0;
    while mi_row < mi_rows {
        let mut mi_col = 0;
        while mi_col < mi_cols {
            for (buf, stride, ss_x, ss_y) in planes.iter_mut() {
                let base_x = (mi_col * 8) >> *ss_x;
                let base_y = (mi_row * 8) >> *ss_y;
                filter_block_plane(
                    buf, *stride, base_x, base_y, *ss_x, *ss_y, &lf, mi_grid, mi_rows, mi_cols,
                    mi_row, mi_col, bd,
                );
            }
            mi_col += MI_BLOCK_SIZE;
        }
        mi_row += MI_BLOCK_SIZE;
    }
}
