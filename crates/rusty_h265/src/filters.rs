//! In-loop filters, applied to a whole picture after all its slices decoded:
//! deblocking (§8.7.2) then sample adaptive offset (§8.7.3).
//!
//! Deblocking works on the 8×8 luma grid over the TU/PU edges the syntax
//! marked in [`PicState::edges`]; boundary strength, decisions and the
//! strong/normal filters follow §8.7.2.4–8.7.2.5.8 literally. SAO reads the
//! deblocked picture (a copy) and writes the final one.

use crate::accel;
use crate::decoder::CurrentPicture;
use crate::frame::Plane;
use crate::pic::{PicState, PRED_INTRA};
use crate::ps::{Pps, Sps};
use crate::tables::{BETA_TABLE, CHROMA_QP_420, TC_TABLE};

/// Runs deblocking then SAO on the finished picture.
pub fn apply_in_loop_filters(cur: &mut CurrentPicture) {
    // Bring-up switch: `RH265_NO_LF=1` skips both filters (compare against
    // `ffmpeg -skip_loop_filter all`).
    if std::env::var_os("RH265_NO_LF").is_some() {
        return;
    }
    let sps = cur.sps.clone();
    let pps = cur.pps.clone();
    if accel::census::enabled() {
        use accel::census as cx;
        // Picture-level capability routes. These are the ones that decide
        // which whole families of kernel a stream will ever touch.
        cx::route(sps.bit_depth_luma > 8, &cx::RT_PIC_10BIT, &cx::RT_PIC_8BIT);
        if pps.tiles_enabled {
            cx::arm(&cx::RT_PIC_TILES);
        }
        if pps.entropy_coding_sync_enabled {
            cx::arm(&cx::RT_PIC_WPP);
        }
    }
    let any_deblock = cur.state.ctb_filter.iter().any(|f| !f.deblock_disabled);
    if any_deblock {
        let (bs_v, bs_h) = (&mut cur.deblock_bs_v, &mut cur.deblock_bs_h);
        deblock(&mut cur.pic.planes, &cur.state, &sps, &pps, bs_v, bs_h);
    }
    let any_sao = sps.sao_enabled && cur.state.ctb_filter.iter().any(|f| f.sao_luma || f.sao_chroma);
    if any_sao && std::env::var_os("RH265_NO_SAO").is_none() {
        // Whether any coding unit in this picture forbids loop filtering
        // (PCM with `pcm_loop_filter_disabled_flag`, or transquant bypass).
        // Almost never true, and knowing it removes a lookup per sample.
        let has_bypass = cur.state.filter_bypass.iter().any(|&b| b != 0);
        // Route: a picture-level capability gate. Any transquant-bypass or
        // PCM-loop-filter-disabled sample anywhere forces the whole picture
        // onto the per-sample scalar path. One-sided: there is no counter for
        // "not bypassed", because that would double-count the per-CTB interior
        // route below.
        if has_bypass {
            accel::census::arm(&accel::census::RT_SAO_PIC_BYPASS);
        }
        let scratch = &mut cur.sao_scratch;
        sao(&mut cur.pic.planes, &cur.state, &sps, &pps, scratch, has_bypass);
    }
}

/// §8.7.2.4 boundary strength for the edge between the 4×4 blocks `p` and `q`
/// (indices into the per-4×4 maps); `tu_edge` says the edge is a transform
/// block edge.
fn boundary_strength(st: &PicState, p: usize, q: usize, tu_edge: bool) -> u8 {
    if st.pred_mode[p] == PRED_INTRA || st.pred_mode[q] == PRED_INTRA {
        return 2;
    }
    if tu_edge && (st.nz[p] != 0 || st.nz[q] != 0) {
        return 1;
    }
    let mp = &st.motion[p];
    let mq = &st.motion[q];
    let np = mp.pred_flags.count_ones();
    let nq = mq.pred_flags.count_ones();
    if np != nq {
        return 1;
    }
    let far = |a: [i16; 2], b: [i16; 2]| (a[0] as i32 - b[0] as i32).abs() >= 4 || (a[1] as i32 - b[1] as i32).abs() >= 4;
    if np == 1 {
        let (rp, vp) = if mp.pred_flags & 1 != 0 { (mp.ref_poc[0], mp.mv[0]) } else { (mp.ref_poc[1], mp.mv[1]) };
        let (rq, vq) = if mq.pred_flags & 1 != 0 { (mq.ref_poc[0], mq.mv[0]) } else { (mq.ref_poc[1], mq.mv[1]) };
        if rp != rq {
            return 1;
        }
        return far(vp, vq) as u8;
    }
    // two motion vectors each
    let (p0, p1, q0, q1) = (mp.ref_poc[0], mp.ref_poc[1], mq.ref_poc[0], mq.ref_poc[1]);
    let same_set = (p0 == q0 && p1 == q1) || (p0 == q1 && p1 == q0);
    if !same_set {
        return 1;
    }
    if p0 != p1 {
        // distinct reference pictures: compare the vectors that share a picture
        if p0 == q0 {
            (far(mp.mv[0], mq.mv[0]) || far(mp.mv[1], mq.mv[1])) as u8
        } else {
            (far(mp.mv[0], mq.mv[1]) || far(mp.mv[1], mq.mv[0])) as u8
        }
    } else {
        // both vectors point at the same picture
        let straight = far(mp.mv[0], mq.mv[0]) || far(mp.mv[1], mq.mv[1]);
        let crossed = far(mp.mv[0], mq.mv[1]) || far(mp.mv[1], mq.mv[0]);
        (straight && crossed) as u8
    }
}

/// Whether the edge between 4×4 blocks p (left/above) and q may be filtered
/// at all (picture, tile and slice boundaries, disabled slices).
fn edge_allowed(st: &PicState, pps: &Pps, xp: usize, yp: usize, xq: usize, yq: usize) -> bool {
    let cq = st.ctb_of(xq, yq);
    let cp = st.ctb_of(xp, yp);
    let fq = &st.ctb_filter[cq];
    if fq.deblock_disabled {
        return false;
    }
    if cp != cq {
        if st.tile_id[cp] != st.tile_id[cq] && !pps.loop_filter_across_tiles_enabled {
            return false;
        }
        if st.slice_addr[cp] != st.slice_addr[cq] && !fq.lf_across_slices {
            return false;
        }
    }
    true
}

fn deblock(planes: &mut [Plane; 3], st: &PicState, sps: &Sps, pps: &Pps, bs_v: &mut Vec<u8>, bs_h: &mut Vec<u8>) {
    let w4 = st.w4;
    let h4 = st.h4;
    let bd_y = sps.bit_depth_luma;
    let bd_c = sps.bit_depth_chroma;
    // bS per 4×4 for its left edge (vertical) and top edge (horizontal); 0 = none.
    //
    // Reused across pictures rather than allocated per picture: at 720p these
    // are 56 KB each, so a fresh pair per picture is 115 KB of `calloc` and
    // memset that the loop below overwrites anyway. `resize` keeps the
    // clearing, which IS needed — the loop only writes entries whose edge is
    // marked, and reads every entry.
    bs_v.clear();
    bs_h.clear();
    bs_v.resize(w4 * h4, 0);
    bs_h.resize(w4 * h4, 0);
    for y4 in 0..h4 {
        for x4 in 0..w4 {
            let i = y4 * w4 + x4;
            let e = st.edges[i];
            if st.pred_mode[i] == 0 {
                continue;
            }
            // vertical edge on the 8-grid
            if x4 > 0 && x4 % 2 == 0 && e & 0b0101 != 0 {
                let p = i - 1;
                if st.pred_mode[p] != 0 && edge_allowed(st, pps, x4 * 4 - 1, y4 * 4, x4 * 4, y4 * 4) {
                    bs_v[i] = boundary_strength(st, p, i, e & 1 != 0);
                }
            }
            if y4 > 0 && y4 % 2 == 0 && e & 0b1010 != 0 {
                let p = i - w4;
                if st.pred_mode[p] != 0 && edge_allowed(st, pps, x4 * 4, y4 * 4 - 1, x4 * 4, y4 * 4) {
                    bs_h[i] = boundary_strength(st, p, i, e & 2 != 0);
                }
            }
        }
    }
    // One strided pass per direction, luma and chroma together.
    //
    // The scan, not the filtering, was the cost here. Deblocking measured 13.3%
    // of decode, and the arithmetic says why: at 720p this is 57,600 4x4 blocks
    // a frame, and the old shape walked every one of them FOUR times per frame
    // -- a luma pass and a chroma pass for each direction -- to reach 460,672
    // actual filter calls across the clip. **37.5 blocks scanned per call**, and
    // ~86M loop iterations against ~40M ops of real filtering.
    //
    // Almost all of it was provably empty. `bs_v[i]` is only ever WRITTEN where
    // `x4 % 2 == 0`, and `bs_h[i]` only where `y4 % 2 == 0` (see the loop
    // above) -- every other entry is the zero left by `resize`, and the scan
    // existed to read those zeros. Chroma is stricter still, `% 4`, which makes
    // its positions a SUBSET of luma's: the two passes can share one walk.
    //
    // So: stride the axis the direction constrains, and fold chroma into the
    // luma pass. Four full scans become one half-density scan per direction --
    // 4x fewer iterations -- with the order within each plane unchanged, and
    // luma and chroma are different planes so interleaving them cannot interact.
    for dir in 0..2 {
        let bs = if dir == 0 { &*bs_v } else { &*bs_h };
        // Vertical edges live on even columns, horizontal edges on even rows.
        let (ys, ystep, xs, xstep) = if dir == 0 { (0, 1, 2, 2) } else { (2, 2, 0, 1) };
        let mut y4 = ys;
        while y4 < h4 {
            let mut x4 = xs;
            while x4 < w4 {
                let i = y4 * w4 + x4;
                let b = bs[i];
                if b == 0 {
                    x4 += xstep;
                    continue;
                }
                let p = if dir == 0 { i - 1 } else { i - w4 };
                let f = &st.ctb_filter[st.ctb_of(x4 * 4, y4 * 4)];
                let qp = (st.qp_y[p] as i32 + st.qp_y[i] as i32 + 1) >> 1;
                let no_p = st.filter_bypass[p] != 0;
                let no_q = st.filter_bypass[i] != 0;

                let qb = (qp + ((f.beta_offset_div2 as i32) << 1)).clamp(0, 51);
                let beta = (BETA_TABLE[qb as usize] as i32) << (bd_y - 8);
                let qt = (qp + 2 * (b as i32 - 1) + ((f.tc_offset_div2 as i32) << 1)).clamp(0, 53);
                let tc = (TC_TABLE[qt as usize] as i32) << (bd_y - 8);
                accel::census::arm(&accel::census::RT_DEBLOCK_LUMA);
                let pl = &mut planes[0];
                let (st_, mx) = (pl.stride, (1i32 << bd_y) - 1);
                accel::deblock::luma_edge(&mut pl.data, st_, x4 * 4, y4 * 4, dir, beta, tc, no_p, no_q, mx);

                // Chroma: bS == 2 on the chroma 8-grid (the luma 16-grid).
                let on_chroma_grid = if dir == 0 { x4 % 4 == 0 } else { y4 % 4 == 0 };
                if b == 2 && on_chroma_grid {
                    accel::census::arm(&accel::census::RT_DEBLOCK_CHROMA);
                    for c in 1..3usize {
                        let off = if c == 1 { pps.cb_qp_offset } else { pps.cr_qp_offset };
                        let qpi = qp + off;
                        let qpc = if qpi < 0 { qpi } else if qpi > 57 { qpi - 6 } else { CHROMA_QP_420[qpi as usize] as i32 };
                        let qt = (qpc + 2 + ((f.tc_offset_div2 as i32) << 1)).clamp(0, 53);
                        let tc = (TC_TABLE[qt as usize] as i32) << (bd_c - 8);
                        filter_chroma_edge(&mut planes[c], x4 * 2, y4 * 2, dir, tc, no_p, no_q, bd_c);
                    }
                }
                x4 += xstep;
            }
            y4 += ystep;
        }
    }
}

/// §8.7.2.5.5 / 8.7.2.5.8 for one 2-line chroma edge segment.
#[allow(clippy::too_many_arguments)]
fn filter_chroma_edge(pl: &mut Plane, x: usize, y: usize, dir: usize, tc: i32, no_p: bool, no_q: bool, bd: u8) {
    let stride = pl.stride;
    let max = (1i32 << bd) - 1;
    // Same lattice as the luma filter; see `filter_luma_edge`.
    let (line_step, tap_step): (isize, isize) = if dir == 0 { (stride as isize, 1) } else { (1, stride as isize) };
    let origin = (y * stride + x) as isize;
    let idx = |k: usize, i: i32| -> usize { (origin + k as isize * line_step + i as isize * tap_step) as usize };
    for k in 0..2 {
        let p0 = pl.data[idx(k, -1)] as i32;
        let p1 = pl.data[idx(k, -2)] as i32;
        let q0 = pl.data[idx(k, 0)] as i32;
        let q1 = pl.data[idx(k, 1)] as i32;
        let delta = ((((q0 - p0) << 2) + p1 - q1 + 4) >> 3).clamp(-tc, tc);
        if !no_p {
            pl.data[idx(k, -1)] = (p0 + delta).clamp(0, max) as u16;
        }
        if !no_q {
            pl.data[idx(k, 0)] = (q0 - delta).clamp(0, max) as u16;
        }
    }
}

/// Bring-up switch: `RH265_SCALAR_SAO=1` keeps SAO on the scalar reference
/// loops. Both arms of the kernel A/B are then the SAME binary, which removes
/// the build-difference and stale-binary questions from the measurement.
fn scalar_sao() -> bool {
    static F: std::sync::OnceLock<bool> = std::sync::OnceLock::new();
    *F.get_or_init(|| std::env::var_os("RH265_SCALAR_SAO").is_some())
}

/// §8.7.3 sample adaptive offset over the deblocked picture.
///
/// The shape here is the same lesson as motion compensation: the per-sample
/// work is trivial, so what costs is everything wrapped around it. Three
/// things are hoisted out of the inner loop.
///
/// - **The source copy.** SAO reads deblocked samples while writing filtered
///   ones, so it needs a copy — but one reused buffer, not a fresh clone of
///   every plane of every picture.
/// - **The loop-filter bypass check.** `pcm_loop_filter_disabled_flag` and
///   `cu_transquant_bypass_flag` are rare; when no coding unit in the picture
///   sets either, the check does not belong in a per-sample loop at all.
/// - **Neighbour availability.** Only samples on the one-sample ring at the
///   edge of a coding tree block can have an unusable neighbour. The interior
///   — which is all but `4·cs` of `cs²` samples — needs no check whatsoever.
fn sao(planes: &mut [Plane; 3], st: &PicState, sps: &Sps, pps: &Pps, scratch: &mut Vec<u16>, has_bypass: bool) {
    // Hoisted once per picture: the kernels take a whole rectangle, so they are
    // only usable when no sample in the picture opts out of filtering.
    let use_kernel = !has_bypass && !scalar_sao();
    let ctb = 1usize << st.log2_ctb;
    for c in 0..3usize {
        let ss = if c == 0 { 0 } else { 1 };
        let bd = if c == 0 { sps.bit_depth_luma } else { sps.bit_depth_chroma };
        let max = (1i32 << bd) - 1;
        let (pw, ph, pstride) = {
            let p = &planes[c];
            (p.width, p.height, p.stride)
        };
        if pw == 0 || ph == 0 {
            continue;
        }
        // The deblocked picture, reused across planes and pictures.
        scratch.clear();
        scratch.extend_from_slice(&planes[c].data);
        let src = &scratch[..];
        let dst = &mut planes[c];
        let cs = ctb >> ss;
        for ry in 0..st.ctb_h {
            for rx in 0..st.ctb_w {
                let rs = ry * st.ctb_w + rx;
                if st.slice_addr[rs] < 0 {
                    continue;
                }
                let prm = st.sao[rs][c];
                // Route: SAO off for this CTB, band offset, or edge offset.
                //
                // Dedicated counters, NOT the kernel's. Aliasing them made
                // `SAO_BAND_SIMD` report 224 band-kernel calls on
                // `LS_B_Orange_4`, where the picture-level bypass flag means the
                // band kernel runs exactly zero times — a counter that lies in
                // the same way a never-incremented one does.
                accel::census::arm(match prm.type_idx {
                    0 => &accel::census::RT_SAO_OFF,
                    1 => &accel::census::RT_SAO_BAND,
                    _ => &accel::census::RT_SAO_EDGE,
                });
                if prm.type_idx == 0 {
                    continue;
                }
                let x0 = rx * cs;
                let y0 = ry * cs;
                let x1 = (x0 + cs).min(pw);
                let y1 = (y0 + cs).min(ph);
                let fq = &st.ctb_filter[rs];

                if prm.type_idx == 1 {
                    // Band offset: no neighbours, so nothing to check but the
                    // bypass flag.
                    let mut band = [0i16; 32];
                    for k in 0..4usize {
                        band[(k + prm.aux as usize) & 31] = prm.offset[k];
                    }
                    let shift = bd - 5;
                    // With no transquant-bypass samples anywhere in the picture
                    // the rectangle is unconditional, which is what the kernel
                    // wants; otherwise the per-sample check keeps it scalar.
                    if use_kernel {
                        accel::sao::sao_band(&mut dst.data, src, pstride, x0, y0, x1 - x0, y1 - y0, shift as u32, prm.aux, &band, max);
                        continue;
                    }
                    for y in y0..y1 {
                        for x in x0..x1 {
                            if has_bypass && st.filter_bypass[st.idx4(x << ss, y << ss)] != 0 {
                                continue;
                            }
                            let v = src[y * pstride + x] as i32;
                            let off = band[(v >> shift) as usize] as i32;
                            if off != 0 {
                                dst.data[y * pstride + x] = (v + off).clamp(0, max) as u16;
                            }
                        }
                    }
                    continue;
                }

                // Edge offset. `off[k]` is indexed by edgeIdx − 1; category 2
                // (the plateau) has no offset and is skipped by construction.
                let (da, db) = match prm.aux {
                    0 => ((-1i32, 0i32), (1i32, 0i32)),
                    1 => ((0, -1), (0, 1)),
                    2 => ((-1, -1), (1, 1)),
                    _ => ((1, -1), (-1, 1)),
                };
                let offs = prm.offset;
                // Interior: every neighbour of every sample here is inside
                // this same coding tree block, so it is always usable.
                let ix0 = x0 + 1;
                let iy0 = y0 + 1;
                let ix1 = x1.saturating_sub(1);
                let iy1 = y1.saturating_sub(1);
                let interior_ok = ix0 < ix1 && iy0 < iy1;
                // Route: whether the check-free interior kernel was taken.
                // The ring ALWAYS runs afterwards for an edge-offset CTB, so
                // `RT_SAO_RING` counts CTBs that got no interior kernel at
                // all (too small, or a bypass picture) — not ring samples.
                accel::census::route(interior_ok && use_kernel, &accel::census::RT_SAO_INTERIOR, &accel::census::RT_SAO_RING);
                if interior_ok && use_kernel {
                    // The whole point of splitting interior from ring: no
                    // availability test, no bypass test, no clamp on the
                    // neighbour coordinates — a flat rectangle the kernel takes
                    // eight or sixteen samples at a time.
                    accel::sao::sao_edge(&mut dst.data, src, pstride, ix0, iy0, ix1 - ix0, iy1 - iy0, da, db, &offs, max);
                } else if interior_ok {
                    for y in iy0..iy1 {
                        let row = y * pstride;
                        let arow = (y as i32 + da.1) as usize * pstride;
                        let brow = (y as i32 + db.1) as usize * pstride;
                        for x in ix0..ix1 {
                            if has_bypass && st.filter_bypass[st.idx4(x << ss, y << ss)] != 0 {
                                continue;
                            }
                            let v = src[row + x] as i32;
                            let a = src[(arow as i32 + x as i32 + da.0) as usize] as i32;
                            let b = src[(brow as i32 + x as i32 + db.0) as usize] as i32;
                            let e = 2 + (v - a).signum() + (v - b).signum();
                            let k = EDGE_CATEGORY[e as usize];
                            if k != 0 {
                                let off = offs[k as usize - 1] as i32;
                                dst.data[row + x] = (v + off).clamp(0, max) as u16;
                            }
                        }
                    }
                }
                // The ring: picture edges, slice and tile boundaries.
                for y in y0..y1 {
                    let interior_row = interior_ok && y >= iy0 && y < iy1;
                    let mut x = x0;
                    while x < x1 {
                        if interior_row && x == ix0 {
                            x = ix1;
                            continue;
                        }
                        if !(has_bypass && st.filter_bypass[st.idx4(x << ss, y << ss)] != 0) {
                            let (ax, ay) = (x as i32 + da.0, y as i32 + da.1);
                            let (bx, by) = (x as i32 + db.0, y as i32 + db.1);
                            let inside = ax >= 0 && ay >= 0 && bx >= 0 && by >= 0 && ax < pw as i32 && ay < ph as i32 && bx < pw as i32 && by < ph as i32;
                            if inside
                                && sao_neighbour_usable(st, pps, fq.lf_across_slices, x << ss, y << ss, (ax as usize) << ss, (ay as usize) << ss)
                                && sao_neighbour_usable(st, pps, fq.lf_across_slices, x << ss, y << ss, (bx as usize) << ss, (by as usize) << ss)
                            {
                                let v = src[y * pstride + x] as i32;
                                let a = src[ay as usize * pstride + ax as usize] as i32;
                                let b = src[by as usize * pstride + bx as usize] as i32;
                                let e = 2 + (v - a).signum() + (v - b).signum();
                                let k = EDGE_CATEGORY[e as usize];
                                if k != 0 {
                                    let off = offs[k as usize - 1] as i32;
                                    dst.data[y * pstride + x] = (v + off).clamp(0, max) as u16;
                                }
                            }
                        }
                        x += 1;
                    }
                }
            }
        }
    }
}

/// `edgeIdx` (Table 8-19) mapped to an offset index, `0` meaning "no offset":
/// the sum `2 + sign(v−a) + sign(v−b)` indexes this directly.
static EDGE_CATEGORY: [u8; 5] = [1, 2, 0, 3, 4];
/// §8.7.3.2: a neighbouring sample across a slice/tile boundary may be
/// unusable, in which case the edge offset is not applied.
fn sao_neighbour_usable(st: &PicState, pps: &Pps, cur_lf_across: bool, xc: usize, yc: usize, xn: usize, yn: usize) -> bool {
    let cc = st.ctb_of(xc, yc);
    let cn = st.ctb_of(xn, yn);
    if cc == cn {
        return true;
    }
    if st.slice_addr[cn] < 0 {
        return false;
    }
    if st.slice_addr[cn] != st.slice_addr[cc] {
        let n_before = st.zs[st.idx4(xn, yn)] < st.zs[st.idx4(xc, yc)];
        if n_before && !cur_lf_across {
            return false;
        }
        if !n_before && !st.ctb_filter[cn].lf_across_slices {
            return false;
        }
    }
    if st.tile_id[cn] != st.tile_id[cc] && !pps.loop_filter_across_tiles_enabled {
        return false;
    }
    true
}

#[cfg(test)]
mod deblock_algebra {
    //! The factored deblocking filters against the specification's own form.
    //!
    //! These are exact integer rewrites, so the gate is equality, not tolerance.
    //! The conformance corpus exercises them too, but only on the sample values
    //! real streams produce; these sweep the whole domain including the extremes
    //! where a factoring mistake would first show.

    /// §8.7.2.5.7 transcribed literally from the specification.
    fn spec_strong(p: [i32; 4], q: [i32; 4]) -> [i32; 6] {
        let (p3, p2, p1, p0) = (p[3], p[2], p[1], p[0]);
        let (q0, q1, q2, q3) = (q[0], q[1], q[2], q[3]);
        [
            (2 * p3 + 3 * p2 + p1 + p0 + q0 + 4) >> 3,
            (p2 + p1 + p0 + q0 + 2) >> 2,
            (p2 + 2 * p1 + 2 * p0 + 2 * q0 + q1 + 4) >> 3,
            (p1 + 2 * p0 + 2 * q0 + 2 * q1 + q2 + 4) >> 3,
            (p0 + q0 + q1 + q2 + 2) >> 2,
            (p0 + q0 + q1 + 3 * q2 + 2 * q3 + 4) >> 3,
        ]
    }

    /// The factored form the filter actually runs.
    fn factored_strong(p: [i32; 4], q: [i32; 4]) -> [i32; 6] {
        let (p3, p2, p1, p0) = (p[3], p[2], p[1], p[0]);
        let (q0, q1, q2, q3) = (q[0], q[1], q[2], q[3]);
        let sp = p0 + q0;
        let u = p1 + p2;
        let v = q1 + q2;
        let a = sp + 2;
        let w = 2 * sp + p1 + q1 + 4;
        let b = sp + 4;
        [
            (b + u + 2 * (p2 + p3)) >> 3,
            (a + u) >> 2,
            (w + u) >> 3,
            (w + v) >> 3,
            (a + v) >> 2,
            (b + v + 2 * (q2 + q3)) >> 3,
        ]
    }

    #[test]
    fn strong_filter_factoring_matches_spec() {
        let mut st = 0xdeb1_0c47u32;
        let mut rnd = |s: &mut u32, m: i32| {
            *s = s.wrapping_mul(1_664_525).wrapping_add(1_013_904_223);
            ((*s >> 8) as i32) % (m + 1)
        };
        for &bd in &[8u32, 10, 12, 14] {
            let max = (1i32 << bd) - 1;
            // the corners, where a factoring error shows first
            for &(a, b) in &[(0, 0), (0, max), (max, 0), (max, max)] {
                assert_eq!(spec_strong([a; 4], [b; 4]), factored_strong([a; 4], [b; 4]), "bd={bd} flat {a}/{b}");
            }
            for _ in 0..20000 {
                let p = [rnd(&mut st, max), rnd(&mut st, max), rnd(&mut st, max), rnd(&mut st, max)];
                let q = [rnd(&mut st, max), rnd(&mut st, max), rnd(&mut st, max), rnd(&mut st, max)];
                assert_eq!(spec_strong(p, q), factored_strong(p, q), "bd={bd} p={p:?} q={q:?}");
            }
        }
    }

    /// The weak filter's `9x - 3y` strength reduction, over the SIGNED domain --
    /// both operands are differences, so negatives are the common case and the
    /// arithmetic shift is what makes the rewrite exact.
    #[test]
    fn weak_delta_strength_reduction_matches_spec() {
        for bd in [8i32, 10, 12] {
            let max = (1i32 << bd) - 1;
            let pts = [-max, -max / 2, -3, -1, 0, 1, 3, max / 2, max];
            for &da in &pts {
                for &db in &pts {
                    let spec = (9 * da - 3 * db + 8) >> 4;
                    let t = da + (da << 1) - db;
                    let fast = (t + (t << 1) + 8) >> 4;
                    assert_eq!(spec, fast, "bd={bd} da={da} db={db}");
                }
            }
        }
    }
}
