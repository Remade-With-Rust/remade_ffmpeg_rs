//! Transform tree (§7.3.8.8), transform unit (§7.3.8.10), residual coding
//! (§7.3.8.11) and the reconstruction path: intra prediction glue, scaling
//! and inverse transform, residual add. A child of `ctu` so it shares the
//! `SliceDecoder` fields.

use super::{wrap_qp, PartMode, SliceDecoder};
use crate::cabac::*;
use rusty_h265_accel as accel;
use crate::error::{Error, Result};
use crate::intra;
use crate::itx::{self, TransformKind};
use crate::pic::{PicState, PRED_INTRA};
use crate::tables::{scan_order, CHROMA_QP_420, SIG_CTX_MAP_4X4};

impl<'a> SliceDecoder<'a> {
    #[allow(clippy::too_many_arguments)]
    pub(super) fn transform_tree(
        &mut self,
        x0: usize,
        y0: usize,
        xbase: usize,
        ybase: usize,
        log2: usize,
        depth: u8,
        blk_idx: usize,
        parent_cbf_cb: bool,
        parent_cbf_cr: bool,
    ) -> Result<()> {
        let max_tb = self.sps.log2_max_tb_size as usize;
        let min_tb = self.sps.log2_min_tb_size as usize;
        let split = if log2 <= max_tb && log2 > min_tb && depth < self.max_trafo_depth && !(self.intra_split && depth == 0) {
            self.cab.decode(CTX_SPLIT_TRANSFORM + 5 - log2) == 1
        } else {
            let inter_split = self.sps.max_transform_hierarchy_depth_inter == 0
                && !self.cu_intra
                && self.part_mode != PartMode::Part2Nx2N
                && depth == 0;
            log2 > max_tb || (self.intra_split && depth == 0) || inter_split
        };
        let (mut cbf_cb, mut cbf_cr) = (false, false);
        if log2 > 2 {
            if depth == 0 || parent_cbf_cb {
                cbf_cb = self.cab.decode(CTX_CBF_CHROMA + depth as usize) == 1;
            }
            if depth == 0 || parent_cbf_cr {
                cbf_cr = self.cab.decode(CTX_CBF_CHROMA + depth as usize) == 1;
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
        if self.cu_intra || depth != 0 || cbf_cb || cbf_cr {
            cbf_luma = self.cab.decode(CTX_CBF_LUMA + if depth == 0 { 1 } else { 0 }) == 1;
        }
        self.transform_unit(x0, y0, xbase, ybase, log2, blk_idx, cbf_luma, cbf_cb, cbf_cr)
    }

    #[allow(clippy::too_many_arguments)]
    fn transform_unit(
        &mut self,
        x0: usize,
        y0: usize,
        xbase: usize,
        ybase: usize,
        log2: usize,
        blk_idx: usize,
        cbf_luma: bool,
        cbf_cb: bool,
        cbf_cr: bool,
    ) -> Result<()> {
        let n = 1usize << log2;
        let cbf_chroma = cbf_cb || cbf_cr;
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
        self.mark_edges(x0, y0, n, n, 0);
        // Luma
        let luma_mode = self.st.intra_mode[self.st.idx4(x0, y0)];
        let mut dc_luma = None;
        if self.cu_intra {
            dc_luma = self.intra_predict(0, x0, y0, n, luma_mode, cbf_luma);
        }
        if cbf_luma {
            self.residual_block(x0, y0, log2, 0, luma_mode, dc_luma)?;
            let w4 = self.st.w4;
            PicState::fill4(&mut self.st.nz, w4, x0, y0, n, n, 1);
        }
        // Chroma (4:2:0)
        let (xc, yc, log2c, do_chroma) = if log2 > 2 {
            (x0 / 2, y0 / 2, log2 - 1, true)
        } else if blk_idx == 3 {
            (xbase / 2, ybase / 2, 2, true)
        } else {
            (0, 0, 0, false)
        };
        if do_chroma {
            let nc = 1usize << log2c;
            let cmode = self.intra_chroma_mode;
            for c in 1..3usize {
                let cbf = if c == 1 { cbf_cb } else { cbf_cr };
                let mut dc_c = None;
                if self.cu_intra {
                    dc_c = self.intra_predict(c, xc, yc, nc, cmode, cbf);
                }
                if cbf {
                    self.residual_block(xc * 2, yc * 2, log2c, c, cmode, dc_c)?;
                }
            }
        }
        Ok(())
    }

    // ---- intra prediction glue (§8.4.4.2.1) ----

    /// Predicts the N×N block of component `c` at component coordinates (xb, yb).
    fn intra_predict(&mut self, c: usize, xb: usize, yb: usize, n: usize, mode: u8, residual_follows: bool) -> Option<u16> {
        if self.ablate.intra {
            return None;
        }
        let ss = if c > 0 { 1 } else { 0 };
        let xl = (xb << ss) as i32;
        let yl = (yb << ss) as i32;
        let constrained = self.pps.constrained_intra_pred;
        // Reused across blocks; only the availability flags are cleared.
        let refs = &mut self.iref;
        refs.reset(n);
        let avail = |st: &PicState, xn: i32, yn: i32| -> bool {
            st.available(xl, yl, xn, yn) && (!constrained || st.pred_mode[st.idx4(xn as usize, yn as usize)] == PRED_INTRA)
        };
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
            // not a `%`. Written as `%` it was a hardware divide per run.
            let run = 4 >> ss;
            let rmask = run - 1;
            let mut k = 0;
            while k < 2 * n {
                let end = (k + run - ((yb + k) & rmask)).min(2 * n);
                if avail(st, (xb as i32 - 1) << ss, ((yb + k) as i32) << ss) {
                    for j in k..end {
                        refs.left[j] = plane.get(xb - 1, yb + j);
                        refs.left_avail[j] = true;
                    }
                } else {
                    all_avail = false;
                }
                k = end;
            }
            let mut k = 0;
            while k < 2 * n {
                let end = (k + run - ((xb + k) & rmask)).min(2 * n);
                if avail(st, ((xb + k) as i32) << ss, (yb as i32 - 1) << ss) {
                    let row = &plane.data[(yb - 1) * plane.stride..];
                    refs.top[k..end].copy_from_slice(&row[xb + k..xb + end]);
                    refs.top_avail[k..end].fill(true);
                } else {
                    all_avail = false;
                }
                k = end;
            }
            if avail(st, (xb as i32 - 1) << ss, (yb as i32 - 1) << ss) {
                refs.corner = plane.get(xb - 1, yb - 1);
                refs.corner_avail = true;
            } else {
                all_avail = false;
            }
        }
        let bit_depth = if c == 0 { self.sps.bit_depth_luma } else { self.sps.bit_depth_chroma };
        // Route: every reference present (the picture interior), or some
        // missing and §8.4.4.2.2 substitution needed (edges, slice and tile
        // boundaries, constrained intra).
        accel::census::route(all_avail, &accel::census::RT_INTRA_ALL_AVAIL, &accel::census::RT_INTRA_SUBSTITUTED);
        refs.substitute(n, bit_depth, all_avail);
        let plane = &mut self.pic.planes[c];
        let stride = plane.stride;
        let off = yb * stride + xb;
        intra::predict(refs, n, mode, c, bit_depth, self.sps.strong_intra_smoothing_enabled, &mut plane.data[off..], stride, residual_follows)
    }

    // ---- residual coding (§7.3.8.11) + reconstruction ----

    /// Parses one transform block's coefficients (luma-domain coordinates
    /// `x0, y0`; for chroma these are the chroma block's coordinates × 2),
    /// scales, inverse-transforms and adds to the picture.
    fn residual_block(&mut self, x0: usize, y0: usize, log2: usize, c_idx: usize, pred_mode_intra: u8, dc: Option<u16>) -> Result<()> {
        let n = 1usize << log2;
        let nn = n * n;
        self.coeffs[..nn].fill(0);
        let cab = &mut self.cab;
        let mut transform_skip = false;
        if self.pps.transform_skip_enabled && !self.cu_transquant_bypass && log2 <= self.pps.log2_max_transform_skip_block_size as usize {
            transform_skip = cab.decode(CTX_TRANSFORM_SKIP + if c_idx > 0 { 1 } else { 0 }) == 1;
        }
        // last_sig_coeff_{x,y}_prefix / suffix
        let (ctx_off, ctx_shift) = if c_idx == 0 { (3 * (log2 - 2) + ((log2 - 1) >> 2), (log2 + 1) >> 2) } else { (15, log2 - 2) };
        let cmax = (log2 << 1) - 1;
        let mut px = 0usize;
        while px < cmax && cab.decode(CTX_LAST_X_PREFIX + ctx_off + (px >> ctx_shift)) == 1 {
            px += 1;
        }
        let mut py = 0usize;
        while py < cmax && cab.decode(CTX_LAST_Y_PREFIX + ctx_off + (py >> ctx_shift)) == 1 {
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
        let scan_idx = if self.cu_intra && (log2 == 2 || (log2 == 3 && c_idx == 0)) {
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
        let sb_scan = scan_order(log2sb, scan_idx);
        let pos_scan = scan_order(2, scan_idx);
        let last_sb = sb_scan
            .iter()
            .position(|&(x, y)| x as usize == last_x >> 2 && y as usize == last_y >> 2)
            .ok_or_else(|| Error::invalid("last sub-block"))?;
        let last_pos = pos_scan
            .iter()
            .position(|&(x, y)| x as usize == last_x & 3 && y as usize == last_y & 3)
            .ok_or_else(|| Error::invalid("last position"))?;

        let mut csbf = [[false; 8]; 8];
        // The non-zero rectangle actually written, which bounds the work the
        // inverse transform has to do (see `itx`'s module docs).
        let (mut nz_w, mut nz_h) = (0usize, 0usize);
        let mut c1: usize = 1;
        let sig_base = CTX_SIG + if c_idx == 0 { 0 } else { 27 };
        let gt1_base = CTX_GT1 + if c_idx == 0 { 0 } else { 16 };
        let gt2_base = CTX_GT2 + if c_idx == 0 { 0 } else { 4 };
        let sign_hiding = self.pps.sign_data_hiding_enabled && !self.cu_transquant_bypass;

        for i in (0..=last_sb).rev() {
            let (xs, ys) = (sb_scan[i].0 as usize, sb_scan[i].1 as usize);
            let right = xs + 1 < nsb && csbf[xs + 1][ys];
            let below = ys + 1 < nsb && csbf[xs][ys + 1];
            let mut infer_sb_dc = false;
            let coded = if i < last_sb && i > 0 {
                let ctx = CTX_CSBF + (right || below) as usize + if c_idx == 0 { 0 } else { 2 };
                infer_sb_dc = true;
                cab.decode(ctx) == 1
            } else {
                true
            };
            csbf[xs][ys] = coded;
            let prev_csbf = right as u8 | ((below as u8) << 1);
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
                    let np = nn_ as usize;
                    let (xp, yp) = (pos_scan[np].0 as usize, pos_scan[np].1 as usize);
                    let xc = (xs << 2) + xp;
                    let yc = (ys << 2) + yp;
                    let s = if np > 0 || !infer_sb_dc {
                        let sig_ctx = if log2 == 2 {
                            SIG_CTX_MAP_4X4[(yc << 2) + xc] as usize
                        } else if xc + yc == 0 {
                            0
                        } else {
                            let mut s = match prev_csbf {
                                0 => {
                                    if xp + yp == 0 {
                                        2
                                    } else if xp + yp < 3 {
                                        1
                                    } else {
                                        0
                                    }
                                }
                                1 => {
                                    if yp == 0 {
                                        2
                                    } else if yp == 1 {
                                        1
                                    } else {
                                        0
                                    }
                                }
                                2 => {
                                    if xp == 0 {
                                        2
                                    } else if xp == 1 {
                                        1
                                    } else {
                                        0
                                    }
                                }
                                _ => 2,
                            };
                            if c_idx == 0 {
                                if xs > 0 || ys > 0 {
                                    s += 3;
                                }
                                if log2 == 3 {
                                    s += if scan_idx == 0 { 9 } else { 15 };
                                } else {
                                    s += 21;
                                }
                            } else if log2 == 3 {
                                s += 9;
                            } else {
                                s += 12;
                            }
                            s
                        };
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
                        sig_pos[nsig] = np as u8;
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
            let mut g1 = [false; 16];
            let mut first_g2: Option<usize> = None;
            let num_c1 = nsig.min(8);
            for &np in sig_pos.iter().take(num_c1) {
                let b = cab.decode(gt1_base + ctx_set * 4 + c1) == 1;
                g1[np as usize] = b;
                if b {
                    c1 = 0;
                    if first_g2.is_none() {
                        first_g2 = Some(np as usize);
                    }
                } else if (1..3).contains(&c1) {
                    c1 += 1;
                }
            }
            let mut g2 = false;
            if first_g2.is_some() {
                g2 = cab.decode(gt2_base + ctx_set) == 1;
            }
            let first_sig = sig_pos[nsig - 1] as usize;
            let last_sig = sig_pos[0] as usize;
            let sign_hidden = sign_hiding && last_sig - first_sig > 3;
            let nsigns = if sign_hidden { nsig - 1 } else { nsig };
            let signs = cab.bypass_bits(nsigns as u32);
            // coeff_abs_level_remaining
            let mut rice = 0u32;
            let mut sum_abs = 0i32;
            for k in 0..nsig {
                let np = sig_pos[k] as usize;
                let is_g2_pos = first_g2 == Some(np);
                let base = 1 + g1[np] as i32 + (is_g2_pos && g2) as i32;
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
                    if abs > 3 * (1 << rice) {
                        rice = (rice + 1).min(4);
                    }
                }
                let neg = k < nsigns && (signs >> (nsigns - 1 - k)) & 1 == 1;
                let mut v = if neg { -abs } else { abs };
                if sign_hidden {
                    sum_abs += abs;
                    if k == nsig - 1 && sum_abs & 1 == 1 {
                        v = -v;
                    }
                }
                let (xp, yp) = (pos_scan[np].0 as usize, pos_scan[np].1 as usize);
                let xc = (xs << 2) + xp;
                let yc = (ys << 2) + yp;
                nz_w = nz_w.max(xc + 1);
                nz_h = nz_h.max(yc + 1);
                self.coeffs[yc * n + xc] = v.clamp(-32768, 32767);
            }
        }
        self.reconstruct_residual(x0, y0, log2, c_idx, transform_skip, nz_w, nz_h, dc)
    }

    /// `coeff_abs_level_remaining` (§9.3.3.11), HM's equivalent form.
    fn coeff_remaining(cab: &mut Cabac, rice: u32) -> Result<i32> {
        let mut prefix = 0u32;
        while prefix < 32 && cab.bypass() == 1 {
            prefix += 1;
        }
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
    fn reconstruct_residual(&mut self, x0: usize, y0: usize, log2: usize, c_idx: usize, transform_skip: bool, nz_w: usize, nz_h: usize, dc: Option<u16>) -> Result<()> {
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
        if kind != TransformKind::Bypass {
            let qp = if c_idx == 0 {
                self.qp_y + sps.qp_bd_offset_y
            } else {
                let off = if c_idx == 1 { self.pps.cb_qp_offset + self.sh.cb_qp_offset } else { self.pps.cr_qp_offset + self.sh.cr_qp_offset };
                let qpi = (self.qp_y + off).clamp(-sps.qp_bd_offset_c, 57);
                let qpc = if qpi < 0 { qpi } else { CHROMA_QP_420[qpi as usize] as i32 };
                qpc + sps.qp_bd_offset_c
            };
            let m: Option<&[u8]> = match self.scaling {
                Some(sf) if !(transform_skip && n > 4) => {
                    let size_id = log2 - 2;
                    let matrix_id = if self.cu_intra { 0 } else { 3 } + c_idx;
                    Some(&sf.f[size_id][matrix_id])
                }
                _ => None,
            };
            // Route: a signalled scaling list, or the flat default.
            accel::census::route(m.is_some(), &accel::census::RT_TX_SCALED, &accel::census::RT_TX_FLAT);
            itx::dequant(&mut self.coeffs[..nn], n, nz_w.clamp(1, n), nz_h.clamp(1, n), qp, bit_depth, m);
        }
        if accel::census::enabled() {
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
        let ss = if c_idx > 0 { 1 } else { 0 };
        let max = (1i32 << bit_depth) - 1;
        let (xb, yb) = (x0 >> ss, y0 >> ss);
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