//! Transform tree (§7.3.8.8), transform unit (§7.3.8.10), residual coding
//! (§7.3.8.11) and the reconstruction path: intra prediction glue, scaling
//! and inverse transform, residual add. A child of `ctu` so it shares the
//! `SliceDecoder` fields.

use super::{wrap_qp, IntraComp, PartMode, SliceDecoder};
use crate::cabac::*;
use crate::error::{Error, Result};
use crate::intra;
use crate::itx::{self, TransformKind};
use crate::pic::{PicState, PRED_INTRA};
use crate::tables::{chroma_qp, scan_set};
use rusty_h265_accel as accel;

/// The significance-context row of `transform_skip_context_enabled_flag`.
static ZERO16: [u8; 16] = [0; 16];

/// The per-block range-extension residual decisions (see
/// `SliceDecoder::rext_residual_tools`).
#[derive(Clone, Copy)]
struct RextResidual {
    rdpcm: Option<bool>,
    ts_ctx: bool,
    /// Rotation applies (still subject to the 4x4 size).
    rotate: bool,
    stat_idx: Option<usize>,
    rice_cap: u32,
}

impl RextResidual {
    const NONE: Self = RextResidual {
        rdpcm: None,
        ts_ctx: false,
        rotate: false,
        stat_idx: None,
        rice_cap: 4,
    };
}

/// What happens to an untransformed (transform-skip or transquant-bypass)
/// residual block after scaling, under the range extensions.
#[derive(Debug, Clone, Copy, Default)]
pub(super) struct Untransformed {
    /// `transform_skip_rotation_enabled_flag`: rotate the 4x4 block by 180°.
    pub rotate: bool,
    /// Residual DPCM: `Some(true)` accumulates down columns, `Some(false)`
    /// along rows (§8.6.8).
    pub rdpcm: Option<bool>,
}

impl<'a> SliceDecoder<'a> {
    /// `parent_cbf_cb` / `parent_cbf_cr` are bit sets: bit 0 is the flag of
    /// the (upper) chroma block, bit 1 the lower one that only 4:2:2 codes.
    #[allow(clippy::too_many_arguments)]
    pub(super) fn transform_tree(&mut self, x0: usize, y0: usize, xbase: usize, ybase: usize, log2: usize, depth: u8, blk_idx: usize, parent_cbf_cb: u8, parent_cbf_cr: u8) -> Result<()> {
        let max_tb = self.sps.log2_max_tb_size as usize;
        let min_tb = self.sps.log2_min_tb_size as usize;
        let split = if log2 <= max_tb && log2 > min_tb && depth < self.max_trafo_depth && !(self.intra_split && depth == 0) {
            self.cab.decode(CTX_SPLIT_TRANSFORM + 5 - log2) == 1
        } else {
            let inter_split = self.sps.max_transform_hierarchy_depth_inter == 0 && !self.cu_intra && self.part_mode != PartMode::Part2Nx2N && depth == 0;
            log2 > max_tb || (self.intra_split && depth == 0) || inter_split
        };
        let (mut cbf_cb, mut cbf_cr) = (0u8, 0u8);
        if log2 > 2 || self.cat == 3 {
            // No chroma cbf at all when ChromaArrayType is 0: both stay 0
            // here, at the root or above, so every child inherits 0 (the
            // parents only gate whether a flag is CODED). The luma-cbf
            // condition below is then the version-1 one unchanged.
            if self.cat == 2 {
                (cbf_cb, cbf_cr) = self.cbf_chroma_422(log2, depth, split, parent_cbf_cb, parent_cbf_cr);
            } else if self.chroma {
                let ctx = CTX_CBF_CHROMA + depth as usize;
                if depth == 0 || parent_cbf_cb & 1 != 0 {
                    cbf_cb = self.cab.decode(ctx) as u8;
                }
                if depth == 0 || parent_cbf_cr & 1 != 0 {
                    cbf_cr = self.cab.decode(ctx) as u8;
                }
            }
        } else {
            cbf_cb = parent_cbf_cb;
            cbf_cr = parent_cbf_cr;
        }
        if split {
            let half = 1usize << (log2 - 1);
            self.transform_tree(x0, y0, x0, y0, log2 - 1, depth + 1, 0, cbf_cb, cbf_cr)?;
            self.transform_tree(x0 + half, y0, x0, y0, log2 - 1, depth + 1, 1, cbf_cb, cbf_cr)?;
            self.transform_tree(x0, y0 + half, x0, y0, log2 - 1, depth + 1, 2, cbf_cb, cbf_cr)?;
            self.transform_tree(x0 + half, y0 + half, x0, y0, log2 - 1, depth + 1, 3, cbf_cb, cbf_cr)?;
            return Ok(());
        }
        let mut cbf_luma = true;
        if self.cu_intra || depth != 0 || cbf_cb != 0 || cbf_cr != 0 {
            cbf_luma = self.cab.decode(CTX_CBF_LUMA + if depth == 0 { 1 } else { 0 }) == 1;
        }
        self.transform_unit(x0, y0, xbase, ybase, log2, blk_idx, cbf_luma, cbf_cb, cbf_cr)
    }

    #[allow(clippy::too_many_arguments)]
    fn transform_unit(&mut self, x0: usize, y0: usize, xbase: usize, ybase: usize, log2: usize, blk_idx: usize, cbf_luma: bool, cbf_cb: u8, cbf_cr: u8) -> Result<()> {
        let n = 1usize << log2;
        let cbf_chroma = cbf_cb != 0 || cbf_cr != 0;
        if (cbf_luma || cbf_chroma) && self.pps.cu_qp_delta_enabled && !self.is_cu_qp_delta_coded {
            // cu_qp_delta_abs: prefix TU cMax 5 (ctx 0 then 1), suffix EG0.
            let mut v = 0u32;
            while v < 5 && self.cab.decode(CTX_CU_QP_DELTA + if v == 0 { 0 } else { 1 }) == 1 {
                v += 1;
            }
            if v == 5 {
                v += self.eg_k(0)?;
            }
            let mut delta = v as i32;
            if delta != 0 && self.cab.bypass() == 1 {
                delta = -delta;
            }
            self.is_cu_qp_delta_coded = true;
            self.cu_qp_delta_val = delta;
            let lo = -(26 + self.sps.qp_bd_offset_y / 2);
            let hi = 25 + self.sps.qp_bd_offset_y / 2;
            if delta < lo || delta > hi {
                return Err(Error::invalid("CuQpDeltaVal out of range"));
            }
            self.qp_y = wrap_qp(self.qp_y_pred + delta, self.sps.qp_bd_offset_y);
            self.set_cu_qp();
        }
        if self.cu_chroma_qp_offset_enabled && cbf_chroma && !self.cu_transquant_bypass && !self.is_cu_chroma_qp_offset_coded {
            self.chroma_qp_offset_syntax();
        }
        // Luma
        let luma_mode = self.st.cell[self.st.idx4(x0, y0)].intra_mode;
        let mut dc_luma = None;
        if self.cu_intra {
            dc_luma = self.intra_predict_any(0, x0, y0, n, luma_mode, cbf_luma);
        }
        if cbf_luma {
            self.residual_block_any(x0, y0, log2, 0, luma_mode, dc_luma)?;
            let w4 = self.st.w4;
            PicState::fill4_with(&mut self.st.cell, w4, x0, y0, n, n, |c| c.nz = 1);
        }
        // This transform block's edges, now that its `nz` is known. The sides
        // on the coding block boundary belong to `coding_unit`.
        self.bs_edges(x0, y0, n, n, x0 != self.cu_x, y0 != self.cu_y, true);
        // Chroma, in chroma-plane coordinates (none in 4:0:0). A luma 4x4
        // block below 4:4:4 carries no chroma of its own: the fourth one
        // (blkIdx 3) codes the chroma of the whole 8x8 parent.
        let (sx, sy) = (self.ssx, self.ssy);
        let (xc, yc, log2c, do_chroma) = if !self.chroma {
            (0, 0, 0, false)
        } else if log2 > 2 || self.cat == 3 {
            (x0 >> sx, y0 >> sy, log2 - sx, true)
        } else if blk_idx == 3 {
            (xbase >> sx, ybase >> sy, 2, true)
        } else {
            (0, 0, 0, false)
        };
        if do_chroma {
            let nc = 1usize << log2c;
            let cmode = self.intra_chroma_mode_at(x0, y0);
            // 4:2:2: the chroma block is nc x 2nc, coded as two square
            // blocks, the lower one predicted from the reconstructed upper.
            let nsub = if self.cat == 2 { 2 } else { 1 };
            for c in 1..3usize {
                let cbf = if c == 1 { cbf_cb } else { cbf_cr };
                // Cross-component prediction (4:4:4, RExt): chroma residual
                // gains a scaled copy of the luma residual. Coded only when
                // luma has a residual and chroma is inter or intra DM.
                if self.ccp_active {
                    self.res_scale[c - 1] = if cbf_luma && (!self.cu_intra || self.intra_chroma_dm_at(x0, y0)) {
                        self.cross_comp_pred(c - 1)
                    } else {
                        0
                    };
                }
                for s in 0..nsub {
                    let ycs = yc + s * nc;
                    let coded = (cbf >> s) & 1 != 0;
                    let mut dc_c = None;
                    if self.cu_intra {
                        dc_c = self.intra_predict_any(c, xc, ycs, nc, cmode, coded);
                    }
                    if coded {
                        self.residual_block_any(xc, ycs, log2c, c, cmode, dc_c)?;
                    } else if self.ccp_active && self.res_scale[c - 1] != 0 {
                        self.ccp_only(xc, ycs, log2c, c);
                    }
                }
            }
        }
        Ok(())
    }

    /// The range-extension decisions for one residual block: residual DPCM
    /// (explicit `explicit_rdpcm_flag`/`_dir_flag` in inter units, §7.3.8.11;
    /// implicit in intra units predicted purely horizontally or vertically,
    /// §8.6.8), the single significance context of
    /// `transform_skip_context_enabled_flag`, rotation, and the StatCoeff slot
    /// of persistent Rice adaptation (sbType, §9.3.3.11).
    #[cold]
    #[inline(never)]
    fn rext_residual_tools(cab: &mut Cabac, r: &crate::ps::SpsRangeExt, intra: bool, untransformed: bool, c_idx: usize, mode: u8) -> RextResidual {
        let mut rdpcm = None;
        if untransformed {
            if !intra && r.explicit_rdpcm {
                let k = (c_idx > 0) as usize;
                if cab.decode(CTX_EXPLICIT_RDPCM_FLAG + k) == 1 {
                    rdpcm = Some(cab.decode(CTX_EXPLICIT_RDPCM_DIR + k) == 1);
                }
            } else if intra && r.implicit_rdpcm && (mode == 10 || mode == 26) {
                rdpcm = Some(mode == 26);
            }
        }
        let stat_idx = r.persistent_rice_adaptation.then(|| CTX_STAT_COEFF + 2 * (c_idx == 0) as usize + untransformed as usize);
        RextResidual {
            rdpcm,
            ts_ctx: r.transform_skip_context && untransformed,
            rotate: r.transform_skip_rotation && untransformed && intra,
            stat_idx,
            rice_cap: if stat_idx.is_some() { 31 } else { 4 },
        }
    }

    /// The chroma cbfs of a 4:2:2 transform-tree node: a second flag for the
    /// lower chroma block of every node whose chroma is coded at this depth
    /// (unsplit, or 8x8 luma whose 4x4 children share the parent's chroma).
    /// Out of line so the 4:2:0 path keeps its version-1 shape.
    #[inline(never)]
    fn cbf_chroma_422(&mut self, log2: usize, depth: u8, split: bool, parent_cb: u8, parent_cr: u8) -> (u8, u8) {
        let two = !split || log2 == 3;
        let ctx = CTX_CBF_CHROMA + depth as usize;
        let mut flags = [0u8; 2];
        for (f, parent) in flags.iter_mut().zip([parent_cb, parent_cr]) {
            if depth == 0 || parent & 1 != 0 {
                *f = self.cab.decode(ctx) as u8;
                if two {
                    *f |= (self.cab.decode(ctx) as u8) << 1;
                }
            }
        }
        (flags[0], flags[1])
    }

    /// `cross_comp_pred()` (§7.3.8.12): `ResScaleVal` for chroma component
    /// `k` (0 = Cb, 1 = Cr).
    #[cold]
    #[inline(never)]
    fn cross_comp_pred(&mut self, k: usize) -> i32 {
        // log2_res_scale_abs_plus1: truncated rice, cMax 4, ctxInc 4k + binIdx.
        let mut v = 0usize;
        while v < 4 && self.cab.decode(CTX_LOG2_RES_SCALE_ABS + 4 * k + v) == 1 {
            v += 1;
        }
        if v == 0 {
            return 0;
        }
        let sign = self.cab.decode(CTX_RES_SCALE_SIGN + k) == 1;
        let mag = 1i32 << (v - 1);
        if sign {
            -mag
        } else {
            mag
        }
    }

    /// A chroma block with no coded residual under cross-component
    /// prediction: its residual is the scaled luma residual alone.
    #[cold]
    #[inline(never)]
    fn ccp_only(&mut self, xc: usize, yc: usize, log2: usize, c_idx: usize) {
        let n = 1usize << log2;
        self.coeffs[..n * n].fill(0);
        self.modify_residual(n, c_idx, Untransformed::default());
        let max = (1i32 << self.sps.bit_depth_chroma) - 1;
        let plane = &mut self.pic.planes[c_idx];
        let stride = plane.stride;
        accel::pixel::add_residual(&mut plane.data[yc * stride + xc..], stride, &self.coeffs[..n * n], n, n, max);
    }

    /// The range-extension residual modifications, on `self.coeffs` after the
    /// inverse transform (or scaling-only, or bypass): rotation (§8.6.2),
    /// residual DPCM (§8.6.8), and cross-component prediction (§8.6.6), which
    /// also records the luma residual it reads.
    #[cold]
    #[inline(never)]
    fn modify_residual(&mut self, n: usize, c_idx: usize, ut: Untransformed) {
        let nn = n * n;
        let r = &mut self.coeffs[..nn];
        if ut.rotate {
            // A permutation, so it commutes with the transform-skip shift
            // the spec applies after it.
            r.reverse();
        }
        match ut.rdpcm {
            Some(true) => {
                for i in n..nn {
                    r[i] += r[i - n];
                }
            }
            Some(false) => {
                for y in 0..n {
                    for x in 1..n {
                        r[y * n + x] += r[y * n + x - 1];
                    }
                }
            }
            None => {}
        }
        if self.ccp_active {
            if c_idx == 0 {
                self.res_y[..nn].copy_from_slice(r);
            } else {
                let scale = self.res_scale[c_idx - 1];
                if scale != 0 {
                    let (bdy, bdc) = (self.sps.bit_depth_luma, self.sps.bit_depth_chroma);
                    for (v, &y) in r.iter_mut().zip(&self.res_y[..nn]) {
                        *v += (scale * ((y << bdc) >> bdy)) >> 3;
                    }
                }
            }
        }
    }

    /// `cu_chroma_qp_offset_flag` / `cu_chroma_qp_offset_idx` (§7.3.8.10,
    /// RExt): picks `CuQpOffsetCb/Cr` from the PPS lists for the rest of the
    /// chroma QP offset group.
    #[cold]
    #[inline(never)]
    fn chroma_qp_offset_syntax(&mut self) {
        let pps = self.pps;
        let flag = self.cab.decode(CTX_CU_CHROMA_QP_OFFSET_FLAG) == 1;
        self.cu_qp_offset_c = if flag {
            let len_minus1 = pps.cb_qp_offset_list.len().saturating_sub(1) as u32;
            let mut idx = 0u32;
            // Truncated rice, cMax = chroma_qp_offset_list_len_minus1, one context.
            while idx < len_minus1 && self.cab.decode(CTX_CU_CHROMA_QP_OFFSET_IDX) == 1 {
                idx += 1;
            }
            [pps.cb_qp_offset_list[idx as usize], pps.cr_qp_offset_list[idx as usize]]
        } else {
            [0, 0]
        };
        self.is_cu_chroma_qp_offset_coded = true;
    }

    // ---- intra prediction glue (§8.4.4.2.1) ----

    /// Predicts the N×N block of component `c` at component coordinates
    /// (xb, yb): the version-1 instantiation unless the slice needs the
    /// range-extension intra rules (4:2:2 / 4:4:4 geometry or filtering,
    /// `intra_smoothing_disabled_flag`, implicit RDPCM).
    #[inline(always)]
    fn intra_predict_any(&mut self, c: usize, xb: usize, yb: usize, n: usize, mode: u8, residual_follows: bool) -> Option<u16> {
        if self.intra_range {
            self.intra_predict::<true>(c, xb, yb, n, mode, residual_follows)
        } else {
            self.intra_predict::<false>(c, xb, yb, n, mode, residual_follows)
        }
    }

    /// See [`Self::intra_predict_any`].
    fn intra_predict<const RANGE: bool>(&mut self, c: usize, xb: usize, yb: usize, n: usize, mode: u8, residual_follows: bool) -> Option<u16> {
        crate::prof_scope!(crate::prof::Stage::Intra);
        if self.ablate.intra {
            return None;
        }
        // Per-component subsampling and §8.4.4.2 filters: the version-1
        // constants, or (`RANGE`) fixed per component for the slice
        // (`SliceDecoder::intra_comp`), where implicit RDPCM also turns the
        // boundary smoothing off for a transquant-bypass block.
        let (sx, sy, pf) = if RANGE {
            let IntraComp { sx, sy, mut pf } = self.intra_comp[(c > 0) as usize];
            if self.cu_transquant_bypass && self.sps.rext.implicit_rdpcm {
                pf.edge = false;
            }
            (sx, sy, pf)
        } else {
            let ss = (c > 0) as usize;
            let pf = if c == 0 {
                intra::PredFilters::luma(self.sps.strong_intra_smoothing_enabled)
            } else {
                intra::PredFilters::CHROMA
            };
            (ss, ss, pf)
        };
        let xl = (xb << sx) as i32;
        let yl = (yb << sy) as i32;
        let constrained = self.pps.constrained_intra_pred;
        let refs = &mut self.iref;
        // The reference-sample gather asks about up to 4n + 1 neighbours of
        // one block, so the current-block half of §6.4.1 is hoisted hardest
        // here.
        let ac = self.st.avail_at(xl, yl);
        let bit_depth = if c == 0 { self.sps.bit_depth_luma } else { self.sps.bit_depth_chroma };
        if !constrained {
            // FIVE availability answers, not one per run.
            //
            // §6.4.1 is derived per 4x4, but the 4n + 1 reference samples fall
            // into five groups -- left, below-left, corner, above, above-right
            // -- and each group lies inside ONE block of the current block's
            // own size, aligned to it. A whole aligned block is either before
            // or after the current one in z-scan, sits in one coding tree block
            // (so one slice and one tile), and is fully decoded if it is
            // before: its availability is uniform, and one test answers for
            // every run in it. The only non-uniform case is the picture edge,
            // where the below-left and above-right groups can be cut short --
            // the samples past the edge are unavailable and `substitute_spans`
            // fills them exactly as the per-sample rule does.
            //
            // On the all-intra stream the per-run form asked 60.8 M times --
            // nine per block, 2.8 G instructions -- for answers that came in
            // five.
            let st = &*self.st;
            let plane = &self.pic.planes[c];
            let (nlx, nly) = ((n << sx) as i32, (n << sy) as i32);
            let a_left = st.avail_n(&ac, xl - 1, yl);
            let a_bl = st.avail_n(&ac, xl - 1, yl + nly);
            let a_top = st.avail_n(&ac, xl, yl - 1);
            let a_tr = st.avail_n(&ac, xl + nlx, yl - 1);
            let a_corner = st.avail_n(&ac, xl - 1, yl - 1);
            let bl_count = if a_bl { n.min(plane.height - (yb + n)) } else { 0 };
            let tr_count = if a_tr { n.min(plane.width - (xb + n)) } else { 0 };
            if a_left {
                for j in 0..n {
                    refs.left[j] = plane.get(xb - 1, yb + j);
                }
            }
            for j in n..n + bl_count {
                refs.left[j] = plane.get(xb - 1, yb + j);
            }
            if a_top || tr_count > 0 {
                let row = &plane.data[(yb - 1) * plane.stride..];
                if a_top {
                    refs.top[..n].copy_from_slice(&row[xb..xb + n]);
                }
                refs.top[n..n + tr_count].copy_from_slice(&row[xb + n..xb + n + tr_count]);
            }
            if a_corner {
                refs.corner = plane.get(xb - 1, yb - 1);
            }
            let all_avail = a_left && a_corner && a_top && bl_count == n && tr_count == n;
            if accel::census::ALWAYS {
                accel::census::route(all_avail, &accel::census::RT_INTRA_ALL_AVAIL, &accel::census::RT_INTRA_SUBSTITUTED);
            }
            if !all_avail {
                refs.substitute_spans(n, bit_depth, a_left, bl_count, a_corner, a_top, tr_count);
            }
            let plane = &mut self.pic.planes[c];
            let stride = plane.stride;
            let off = yb * stride + xb;
            return intra::predict_filtered(refs, n, mode, pf, bit_depth, &mut plane.data[off..], stride, residual_follows);
        }
        // Constrained intra prediction: availability also depends on each
        // neighbour's prediction mode, which is not uniform over a block, so
        // this keeps the per-run derivation. It is also the oracle for the
        // span form above (`span_substitution_matches_per_sample`).
        refs.reset(n);
        let avail = |st: &PicState, xn: i32, yn: i32| -> bool { st.avail_n(&ac, xn, yn) && st.cell[st.idx4(xn as usize, yn as usize)].pred_mode == PRED_INTRA };
        // Every reference sample present? The gather derives this per run for
        // free, and it lets `substitute` return immediately.
        let mut all_avail = true;
        {
            let plane = &self.pic.planes[c];
            let st = &*self.st;
            // §6.4.1 availability is derived per minimum block (4x4 luma), so
            // it is constant along a run of `4 >> ss` reference samples. The
            // old shape asked per sample: for a 32x32 block that was 129
            // z-scan/slice/tile derivations instead of 33.
            // `run` is 4 or 2 — a power of two — so the wrap below is a mask,
            // not a `%`. Written as `%` it was a hardware divide per run. It
            // differs per axis in 4:2:2.
            let (run_y, run_x) = (4 >> sy, 4 >> sx);
            // Gather a SPAN of consecutive available runs at a time, not a run.
            //
            // Availability is derived per run, but the copy does not have to be.
            // A run is four samples (two for chroma), and
            // `top[k..end].copy_from_slice(..)` on a slice that short is a
            // runtime-length `memcpy` CALL -- up to sixteen of them per
            // transform block per component, which `memcpy_census.py` ranked as
            // the densest remaining copy site in the decoder. Runs are almost
            // always all available (the picture interior), so accumulating them
            // and flushing at each transition turns those sixteen 8-byte calls
            // into ONE of up to 128 bytes, and leaves the edge cases as the only
            // place more than one flush happens.
            //
            // `reset` still pre-clears the availability flags, so a span that is
            // never flushed is already false. Removing that pre-clear and
            // writing the false runs instead was tried and measured 0.982x on
            // intra-heavy content: it replaced two long fills with thirty-two
            // short ones, which is this same defect in the other direction.
            let mut k = 0;
            let mut span: Option<usize> = None;
            while k < 2 * n {
                let end = (k + run_y - ((yb + k) & (run_y - 1))).min(2 * n);
                if avail(st, (xb as i32 - 1) << sx, ((yb + k) as i32) << sy) {
                    span.get_or_insert(k);
                } else {
                    all_avail = false;
                    if let Some(a) = span.take() {
                        // The left column is strided, so the samples are a loop
                        // either way; it is the flag write that coalesces.
                        for j in a..k {
                            refs.left[j] = plane.get(xb - 1, yb + j);
                        }
                        refs.left_avail[a..k].fill(true);
                    }
                }
                k = end;
            }
            if let Some(a) = span.take() {
                for j in a..2 * n {
                    refs.left[j] = plane.get(xb - 1, yb + j);
                }
                refs.left_avail[a..2 * n].fill(true);
            }
            // NOT hoisted out of the loop: at `yb == 0` there is no row above,
            // and `(yb - 1) * stride` underflows. No run is available there, so
            // the slice is never USED -- but it must not be FORMED either.
            let top_row = || &plane.data[(yb - 1) * plane.stride..];
            let mut k = 0;
            let mut span: Option<usize> = None;
            while k < 2 * n {
                let end = (k + run_x - ((xb + k) & (run_x - 1))).min(2 * n);
                if avail(st, ((xb + k) as i32) << sx, (yb as i32 - 1) << sy) {
                    span.get_or_insert(k);
                } else {
                    all_avail = false;
                    if let Some(a) = span.take() {
                        refs.top[a..k].copy_from_slice(&top_row()[xb + a..xb + k]);
                        refs.top_avail[a..k].fill(true);
                    }
                }
                k = end;
            }
            if let Some(a) = span.take() {
                refs.top[a..2 * n].copy_from_slice(&top_row()[xb + a..xb + 2 * n]);
                refs.top_avail[a..2 * n].fill(true);
            }
            if avail(st, (xb as i32 - 1) << sx, (yb as i32 - 1) << sy) {
                refs.corner = plane.get(xb - 1, yb - 1);
                refs.corner_avail = true;
            } else {
                all_avail = false;
            }
        }
        // Route: every reference present (the picture interior), or some
        // missing and §8.4.4.2.2 substitution needed (edges, slice and tile
        // boundaries, constrained intra).
        if accel::census::ALWAYS {
            accel::census::route(all_avail, &accel::census::RT_INTRA_ALL_AVAIL, &accel::census::RT_INTRA_SUBSTITUTED);
        }
        refs.substitute(n, bit_depth, all_avail);
        let plane = &mut self.pic.planes[c];
        let stride = plane.stride;
        let off = yb * stride + xb;
        intra::predict_filtered(refs, n, mode, pf, bit_depth, &mut plane.data[off..], stride, residual_follows)
    }

    // ---- residual coding (§7.3.8.11) + reconstruction ----

    /// Parses one transform block's coefficients (`x0, y0` in the
    /// component's own sample coordinates), scales, inverse-transforms and
    /// adds to the picture.
    fn residual_block_any(&mut self, x0: usize, y0: usize, log2: usize, c_idx: usize, pred_mode_intra: u8, dc: Option<u16>) -> Result<()> {
        // Two instantiations: without the range-extension residual tools the
        // parse is the version-1 one with every RExt branch compiled out, so
        // a stream that uses none of them pays one predictable branch here.
        if self.rext_residual {
            self.residual_block::<true>(x0, y0, log2, c_idx, pred_mode_intra, dc)
        } else {
            self.residual_block::<false>(x0, y0, log2, c_idx, pred_mode_intra, dc)
        }
    }

    /// One transform block (see [`Self::residual_block_any`]); `REXT` says
    /// whether any range-extension residual tool is on.
    fn residual_block<const REXT: bool>(&mut self, x0: usize, y0: usize, log2: usize, c_idx: usize, pred_mode_intra: u8, dc: Option<u16>) -> Result<()> {
        let n = 1usize << log2;
        let nn = n * n;
        if accel::census::ALWAYS {
            accel::census::bump(&accel::census::RES_BLOCKS, 1);
        }
        // The clear of `self.coeffs` used to be here, before anything was
        // known about the block. It is now below, once the last-significant
        // position says how much of the block is live.
        let cab = &mut self.cab;
        let bypass = self.cu_transquant_bypass;
        let mut transform_skip = false;
        if self.pps.transform_skip_enabled && !bypass && log2 <= self.pps.log2_max_transform_skip_block_size as usize {
            transform_skip = cab.decode(CTX_TRANSFORM_SKIP + if c_idx > 0 { 1 } else { 0 }) == 1;
        }
        // The range-extension residual tools, all behind one per-slice flag
        // so a stream without them pays a single predictable branch.
        let rt = if REXT {
            Self::rext_residual_tools(cab, &self.sps.rext, self.cu_intra, transform_skip || bypass, c_idx, pred_mode_intra)
        } else {
            RextResidual::NONE
        };
        let (rdpcm, ts_ctx, stat_idx, rice_cap) = (rt.rdpcm, rt.ts_ctx, rt.stat_idx, rt.rice_cap);
        // last_sig_coeff_{x,y}_prefix / suffix
        let (ctx_off, ctx_shift) = if c_idx == 0 { (3 * (log2 - 2) + ((log2 - 1) >> 2), (log2 + 1) >> 2) } else { (15, log2 - 2) };
        let cmax = (log2 << 1) - 1;
        // Only `px >> ctx_shift` varies across the run; the base is fixed.
        let (bx, by) = (CTX_LAST_X_PREFIX + ctx_off, CTX_LAST_Y_PREFIX + ctx_off);
        let mut px = 0usize;
        while px < cmax && cab.decode(bx + (px >> ctx_shift)) == 1 {
            px += 1;
        }
        let mut py = 0usize;
        while py < cmax && cab.decode(by + (py >> ctx_shift)) == 1 {
            py += 1;
        }
        let mut last_x = px;
        if px > 3 {
            let nb = (px >> 1) - 1;
            let suffix = cab.bypass_bits(nb as u32) as usize;
            last_x = ((2 + (px & 1)) << nb) + suffix;
        }
        let mut last_y = py;
        if py > 3 {
            let nb = (py >> 1) - 1;
            let suffix = cab.bypass_bits(nb as u32) as usize;
            last_y = ((2 + (py & 1)) << nb) + suffix;
        }
        if last_x >= n || last_y >= n {
            return Err(Error::invalid("last significant coefficient outside the block"));
        }
        // scanIdx (§7.4.9.11)
        let scan_idx = if self.cu_intra && (log2 == 2 || (log2 == 3 && (c_idx == 0 || self.cat == 3))) {
            if (6..=14).contains(&pred_mode_intra) {
                2
            } else if (22..=30).contains(&pred_mode_intra) {
                1
            } else {
                0
            }
        } else {
            0
        };
        if scan_idx == 2 {
            std::mem::swap(&mut last_x, &mut last_y);
        }
        let log2sb = log2 - 2;
        let nsb = 1usize << log2sb;
        // One selection for all five tables (see `ScanSet`).
        let sc = scan_set(log2sb, scan_idx);
        let sb_scan = sc.sb;
        let pos_scan = sc.pos;
        // Two table lookups where there were two linear searches of the
        // forward scan. `last_x` and `last_y` were range-checked against `n`
        // just above, so both indices are inside their table and the
        // "not found" arms the searches carried were unreachable.
        let last_sb = sc.sb_inv[(last_y >> 2) * nsb + (last_x >> 2)] as usize;
        let last_pos = sc.pos_inv[(last_y & 3) * 4 + (last_x & 3)] as usize;
        if accel::census::ALWAYS {
            accel::census::bump(&accel::census::RES_SCAN_SEARCH, 2);
        }
        // Clear only what can be written, and read back.
        //
        // The parse can only write into sub-blocks at or before `last_sb` in
        // the scan, so `scan_bbox` bounds the written region exactly; and for a
        // real transform the consumers (`dequant`, then stage 1 of the inverse
        // transform) read only inside `nz_w x nz_h`, which sits inside that
        // same box. Transform-skip and transquant-bypass are the two kinds that
        // pass the block through untransformed and therefore read every sample,
        // so those still clear all of it.
        //
        // On a 720p stream that is 11.4 M i32 stores down to the ~3.7 M the
        // last-significant position says are live; on intra-heavy content,
        // 178.8 M down to ~22.6 M.
        let (fw, fh) = if transform_skip || self.cu_transquant_bypass {
            (n, n)
        } else {
            let (bw, bh) = sc.sb_bbox[last_sb];
            ((bw as usize) << 2, (bh as usize) << 2)
        };
        if fw == n {
            self.coeffs[..fh * n].fill(0);
        } else {
            for y in 0..fh {
                self.coeffs[y * n..y * n + fw].fill(0);
            }
        }
        // Reborrow once for the whole parse. Indexing `self.coeffs` inside the
        // coefficient loop reloaded the slice's POINTER AND LENGTH from the
        // struct on every coefficient -- two loads each, plus a bounds check
        // against a length the loop cannot change. Bound here, they are
        // loop-invariant. (`cab` above borrows a different field, so the two
        // reborrows are disjoint.)
        let co = &mut self.coeffs[..nn];
        if accel::census::ALWAYS {
            accel::census::bump(&accel::census::RES_FILL_STORES, (fw * fh) as u64);
        }

        // One `u64` in place of a 64-byte array of `bool`: the sub-block flags
        // are a bitmap, and clearing a register beats clearing 64 bytes on
        // every one of 3.0 M transform blocks. Bit `xs * 8 + ys`.
        let mut csbf = 0u64;
        // Scaling (§8.6.3) is applied HERE, at the one write of each coded
        // coefficient, rather than by a second pass over the live rectangle
        // after the parse. A zero scales to zero, so skipping the rest of the
        // block is exact; and the parser is the only place that knows which
        // positions are non-zero without testing every one of them. On the
        // 20 s 720p clip the separate pass was 239 M instructions, 600 per block.
        //
        // `None` is transquant bypass, where the levels ARE the residual.
        let dq = if self.cu_transquant_bypass {
            None
        } else {
            let sps = self.sps;
            let bit_depth = if c_idx == 0 { sps.bit_depth_luma } else { sps.bit_depth_chroma };
            let qp = if c_idx == 0 {
                self.qp_y + sps.qp_bd_offset_y
            } else {
                let off = if c_idx == 1 {
                    self.pps.cb_qp_offset + self.sh.cb_qp_offset
                } else {
                    self.pps.cr_qp_offset + self.sh.cr_qp_offset
                } + self.cu_qp_offset_c[c_idx - 1];
                let qpi = (self.qp_y + off).clamp(-sps.qp_bd_offset_c, 57);
                chroma_qp(qpi, self.cat) + sps.qp_bd_offset_c
            };
            let m: Option<&[u8]> = match self.scaling {
                Some(sf) if !(transform_skip && n > 4) => {
                    let size_id = log2 - 2;
                    let matrix_id = if self.cu_intra { 0 } else { 3 } + c_idx;
                    Some(sf.get(size_id, matrix_id))
                }
                _ => None,
            };
            // Route: a signalled scaling list, or the flat default.
            if accel::census::ALWAYS {
                accel::census::route(m.is_some(), &accel::census::RT_TX_SCALED, &accel::census::RT_TX_FLAT);
            }
            let bd_shift = bit_depth as i32 + log2 as i32 - 5;
            let scale = crate::tables::LEVEL_SCALE[(qp % 6) as usize] << (qp / 6);
            Some((scale as i64, bd_shift, 1i64 << (bd_shift - 1), m))
        };
        // The non-zero rectangle actually written, which bounds the work the
        // inverse transform has to do (see `itx`'s module docs).
        let (mut nz_w, mut nz_h) = (0usize, 0usize);
        let mut c1: usize = 1;
        let sig_base = CTX_SIG + if c_idx == 0 { 0 } else { 27 };
        let gt1_base = CTX_GT1 + if c_idx == 0 { 0 } else { 16 };
        let gt2_base = CTX_GT2 + if c_idx == 0 { 0 } else { 4 };
        // No sign hiding on bypass blocks or residual-DPCM blocks (§7.3.8.11).
        let sign_hiding = self.pps.sign_data_hiding_enabled && !bypass && rdpcm.is_none();

        for i in (0..=last_sb).rev() {
            let (xs, ys) = (sb_scan[i].0 as usize, sb_scan[i].1 as usize);
            let right = xs + 1 < nsb && (csbf >> ((xs + 1) * 8 + ys)) & 1 != 0;
            let below = ys + 1 < nsb && (csbf >> (xs * 8 + ys + 1)) & 1 != 0;
            let mut infer_sb_dc = false;
            let coded = if i < last_sb && i > 0 {
                let ctx = CTX_CSBF + (right || below) as usize + if c_idx == 0 { 0 } else { 2 };
                infer_sb_dc = true;
                cab.decode(ctx) == 1
            } else {
                true
            };
            csbf |= (coded as u64) << (xs * 8 + ys);
            let prev_csbf = right as usize | ((below as usize) << 1);
            // Everything the significance context needs from this sub-block,
            // hoisted out of the per-coefficient loop below. `sig_row[n]` is
            // the position-dependent term keyed by SCAN POSITION, so the loop
            // never has to recover (xP, yP) or (xC, yC); `sig_off` is the
            // sub-block/size/component term, which the old shape recomputed for
            // every one of 2.3 M scanned positions (18.8 M on intra content).
            // `transform_skip_context_enabled_flag` (RExt) codes every flag of
            // an untransformed block with one context: an all-zero row and an
            // offset to that context, so the per-coefficient loop below keeps
            // its shape and pays nothing for the tool.
            let (sig_row, sig_off) = if REXT && ts_ctx {
                (&ZERO16, CTX_SIG_TS + (c_idx > 0) as usize - sig_base)
            } else if log2 == 2 {
                (sc.sig_4x4, 0usize)
            } else {
                let mut o = if xs > 0 || ys > 0 { 3 } else { 0 };
                if c_idx == 0 {
                    o += if log2 == 3 {
                        if scan_idx == 0 {
                            9
                        } else {
                            15
                        }
                    } else {
                        21
                    };
                } else {
                    // The +3 above is luma-only.
                    o = if log2 == 3 { 9 } else { 12 };
                }
                (&sc.sig_nb[prev_csbf & 3], o)
            };
            // The DC of the DC sub-block is context 0 (§9.3.4.2.5). For a 4x4
            // block the map already reads 0 there, so one test serves both.
            let dc_sb = xs == 0 && ys == 0 && !(REXT && ts_ctx);
            // significant_coeff_flag
            let mut sig_pos = [0u8; 16];
            let mut nsig = 0usize;
            let start_n: i32 = if i == last_sb {
                sig_pos[0] = last_pos as u8;
                nsig = 1;
                last_pos as i32 - 1
            } else {
                15
            };
            if coded {
                let mut nn_ = start_n;
                while nn_ >= 0 {
                    if accel::census::ALWAYS {
                        accel::census::bump(&accel::census::RES_SIG_SCANNED, 1);
                    }
                    let np = nn_ as usize;
                    let s = if np > 0 || !infer_sb_dc {
                        if accel::census::ALWAYS {
                            accel::census::bump(&accel::census::RES_SIG_CTX, 1);
                        }
                        let sig_ctx = if dc_sb && np == 0 { 0 } else { sig_row[np & 15] as usize + sig_off };
                        let b = cab.decode(sig_base + sig_ctx) == 1;
                        if b {
                            infer_sb_dc = false;
                        }
                        b
                    } else {
                        // n == 0 with inferSbDcSigCoeffFlag: inferred 1
                        true
                    };
                    if s {
                        sig_pos[nsig & 15] = np as u8;
                        nsig += 1;
                    }
                    nn_ -= 1;
                }
            }
            if nsig == 0 {
                continue;
            }
            // coeff_abs_level_greater1_flag (§9.3.4.2.6)
            let ctx_set = (if i == 0 || c_idx > 0 { 0 } else { 2 }) + if c1 == 0 { 1 } else { 0 };
            c1 = 1;
            // A 16-bit set instead of a 16-byte array, cleared and written per
            // sub-block.
            let mut g1 = 0u16;
            // 16 is outside a sub-block's 0..=15 scan positions, so it serves
            // as "none" and the per-coefficient test below is one compare
            // rather than an `Option` discriminant plus a payload compare.
            let mut first_g2: usize = 16;
            let num_c1 = nsig.min(8);
            // The context SET is fixed for the whole run; only `c1` moves.
            let g1_ctx = gt1_base + ctx_set * 4;
            for &np in sig_pos.iter().take(num_c1) {
                let b = cab.decode(g1_ctx + c1) == 1;
                g1 |= (b as u16) << np;
                if b {
                    c1 = 0;
                    if first_g2 == 16 {
                        first_g2 = np as usize;
                    }
                } else if (1..3).contains(&c1) {
                    c1 += 1;
                }
            }
            let mut g2 = false;
            if first_g2 != 16 {
                g2 = cab.decode(gt2_base + ctx_set) == 1;
            }
            let first_sig = sig_pos[(nsig - 1) & 15] as usize;
            let last_sig = sig_pos[0] as usize;
            let sign_hidden = sign_hiding && last_sig - first_sig > 3;
            let nsigns = if sign_hidden { nsig - 1 } else { nsig };
            // Pre-align the sign bits so each coefficient reads the top one.
            //
            // The old form recovered bit `nsigns - 1 - k` per coefficient: two
            // subtracts, a variable shift and a mask, plus a `k < nsigns`
            // guard. Left-aligned, the sign is the sign bit of an `i32` and the
            // guard disappears -- past `nsigns` the shifts have brought in
            // zeros, which is exactly "not negative".
            //
            // `wrapping_shl` masks its operand to 0..=31, so `nsigns == 0`
            // shifts by 0; `signs` is then 0 anyway, so the result still reads
            // as all-positive.
            let mut sbits = cab.bypass_bits(nsigns as u32).wrapping_shl(32 - nsigns as u32);
            // coeff_abs_level_remaining. The Rice parameter starts at 0, or
            // at StatCoeff[sbType] / 4 with persistent Rice adaptation (RExt).
            let mut rice = match stat_idx {
                Some(i) => (cab.ctx.0[i] as u32 / 4).min(rice_cap),
                None => 0,
            };
            // The first `coeff_abs_level_remaining` of the sub-block, for the
            // StatCoeff update after the loop (persistent Rice only).
            let mut first_rem = -1i32;
            let mut sum_abs = 0i32;
            for k in 0..nsig {
                let np = sig_pos[k & 15] as usize;
                let is_g2_pos = first_g2 == np;
                let base = 1 + ((g1 >> np) & 1) as i32 + (is_g2_pos && g2) as i32;
                let threshold = if k < 8 {
                    if is_g2_pos {
                        3
                    } else {
                        2
                    }
                } else {
                    1
                };
                let mut abs = base;
                if base == threshold {
                    let rem = Self::coeff_remaining(cab, rice)?;
                    abs += rem;
                    if stat_idx.is_some() && first_rem < 0 {
                        first_rem = rem;
                    }
                    if abs > 3 * (1 << rice) {
                        rice = (rice + 1).min(rice_cap);
                    }
                }
                let neg = (sbits as i32) < 0;
                sbits <<= 1;
                let mut v = if neg { -abs } else { abs };
                if sign_hidden {
                    sum_abs += abs;
                    if k == nsig - 1 && sum_abs & 1 == 1 {
                        v = -v;
                    }
                }
                let (xp, yp) = (pos_scan[np & 15].0 as usize, pos_scan[np & 15].1 as usize);
                let xc = (xs << 2) + xp;
                let yc = (ys << 2) + yp;
                nz_w = nz_w.max(xc + 1);
                nz_h = nz_h.max(yc + 1);
                let idx = yc * n + xc;
                let level = v.clamp(-32768, 32767);
                co[idx] = match dq {
                    None => level,
                    Some((scale, bd_shift, add, m)) => {
                        let f = m.map_or(16, |t| t[idx] as i64);
                        (((level as i64 * f * scale) + add) >> bd_shift).clamp(itx::COEFF_MIN as i64, itx::COEFF_MAX as i64) as i32
                    }
                };
            }
            // §9.3.3.11: the sub-block's first remaining level nudges
            // StatCoeff[sbType], which seeds the next sub-block's parameter.
            if let (Some(si), true) = (stat_idx, first_rem >= 0) {
                // Shifts held below 30 so a hostile stream cannot overflow
                // them; a real StatCoeff never gets near (the level would be
                // beyond the 16-bit coefficient range first).
                let s = cab.ctx.0[si];
                let sh = (s / 4).min(29) as u32;
                if first_rem >= (3 << sh) {
                    cab.ctx.0[si] = s.saturating_add(1);
                } else if 2 * first_rem < (1 << sh) && s > 0 {
                    cab.ctx.0[si] = s - 1;
                }
            }
        }
        // §8.6.2: a 4x4 intra block that skips the transform is rotated by
        // 180 degrees under `transform_skip_rotation_enabled_flag`.
        let rotate = rt.rotate && n == 4;
        self.reconstruct_residual(x0, y0, log2, c_idx, transform_skip, nz_w, nz_h, dc, Untransformed { rotate, rdpcm })
    }

    /// `coeff_abs_level_remaining` (§9.3.3.11), HM's equivalent form.
    fn coeff_remaining(cab: &mut Cabac, rice: u32) -> Result<i32> {
        let prefix = cab.bypass_ones(32);
        if prefix >= 32 {
            return Err(Error::invalid("coeff_abs_level_remaining prefix"));
        }
        if prefix < 3 {
            Ok(((prefix << rice) + cab.bypass_bits(rice)) as i32)
        } else {
            let l = prefix - 3;
            if l + rice > 31 {
                return Err(Error::invalid("coeff_abs_level_remaining suffix"));
            }
            Ok(((((1u32 << l) + 2) << rice) + cab.bypass_bits(l + rice)) as i32)
        }
    }

    /// §8.6.2–8.6.4 on `self.coeffs`, then adds the residual to the picture.
    #[allow(clippy::too_many_arguments)]
    fn reconstruct_residual(&mut self, x0: usize, y0: usize, log2: usize, c_idx: usize, transform_skip: bool, nz_w: usize, nz_h: usize, dc: Option<u16>, ut: Untransformed) -> Result<()> {
        crate::prof_scope!(crate::prof::Stage::Transform);
        if self.ablate.residual {
            return Ok(());
        }
        let n = 1usize << log2;
        let nn = n * n;
        let sps = self.sps;
        let bit_depth = if c_idx == 0 { sps.bit_depth_luma } else { sps.bit_depth_chroma };
        let kind = if self.cu_transquant_bypass {
            TransformKind::Bypass
        } else if transform_skip {
            TransformKind::Skip
        } else if self.cu_intra && c_idx == 0 && n == 4 {
            TransformKind::Dst
        } else {
            TransformKind::Dct
        };
        // Scaling already happened at the coefficient write in `residual_block`.
        if accel::census::ALWAYS {
            use accel::census as cx;
            // Route: which inverse transform the block needs.
            cx::arm(match kind {
                TransformKind::Bypass => &cx::RT_TX_BYPASS,
                TransformKind::Skip => &cx::RT_TX_SKIP,
                TransformKind::Dst => &cx::RT_TX_DST,
                TransformKind::Dct => &cx::RT_TX_DCT,
            });
            // Route: block size — each is a different kernel shape.
            cx::arm(match n {
                4 => &cx::RT_TX_N4,
                8 => &cx::RT_TX_N8,
                16 => &cx::RT_TX_N16,
                _ => &cx::RT_TX_N32,
            });
            // Route: sparsity. Not a branch but a continuous population — the
            // ratio is how much of each block the last-significant position
            // actually spared us.
            cx::bump(&cx::RT_TX_NZ_AREA, (nz_w.clamp(1, n) * nz_h.clamp(1, n)) as u64);
            cx::bump(&cx::RT_TX_FULL_AREA, (n * n) as u64);
        }
        itx::inverse_transform(&mut self.coeffs[..nn], &mut self.itx_tmp, n, nz_w, nz_h, bit_depth, kind);
        if ut.rotate || ut.rdpcm.is_some() || self.ccp_active {
            self.modify_residual(n, c_idx, ut);
        }
        let max = (1i32 << bit_depth) - 1;
        let (xb, yb) = (x0, y0);
        // Disjoint field borrows: the coefficients are read, the picture written.
        let coeffs = &self.coeffs[..nn];
        let plane = &mut self.pic.planes[c_idx];
        let stride = plane.stride;
        match dc {
            // The prediction was a single value the fill never wrote; adding
            // the residual to it directly is the whole reconstruction.
            Some(v) => accel::pixel::add_residual_const(&mut plane.data[yb * stride + xb..], stride, v, coeffs, n, n, max),
            None => accel::pixel::add_residual(&mut plane.data[yb * stride + xb..], stride, coeffs, n, n, max),
        }
        Ok(())
    }
}
