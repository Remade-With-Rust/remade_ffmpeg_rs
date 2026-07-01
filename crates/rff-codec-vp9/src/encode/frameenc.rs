//! VP9 encoder — the frame reconstruct loop (Floor 3 intra + Floor 4 inter +
//! Floor 5 rate-distortion brain).
//!
//! [`FrameEncoder`] runs the decoder's reconstruct loop *forward*: it mirrors
//! `decode_partition` → `decode_block` → `reconstruct_plane` →
//! `reconstruct_tx_block` exactly, but at each transform block it *chooses* the
//! mode and *computes* the residual (source − prediction → forward transform →
//! quantize → [`encode_coefs`]) instead of reading them, then reconstructs with
//! the **same** `predict` / motion-compensation + `inverse_transform_add` the
//! decoder uses. Because the reconstruction buffer evolves identically,
//! `decode(encode(frame))` is bit-exact (VP9's determinism).
//!
//! Key frames code every block intra; P frames carry a LAST reference. Mode
//! decisions are **rate-distortion optimised** (Floor 5, R1): each candidate —
//! the intra modes {DC,V,H,TM}, ZEROMV, and a searched + 1/4-pel-refined NEWMV —
//! is trial-coded (via `encode_plane(None)`, reusing the [`coef_cost`] bit oracle)
//! and the one minimising `SSE + λ·bits` is committed. After reconstruction the
//! frame is **deblocked** at a searched `loop_filter_level` (R3), and the tile is
//! re-coded with **forward-adapted coefficient probabilities** (R4: a count→adapt→
//! re-encode pass that signals the deltas in the compressed header). Still a simple
//! controller otherwise: a fixed all-8×8 partition, 4×4 transforms, one frame-wide
//! quantizer (rate control picks it per frame), a single tile.

use super::adapt::{COEF_COUNT_SAT, COEF_MAX_UPDATE_FACTOR};
use super::bitwriter::{BitWriter, BoolEncoder};
use super::compressed::write_compressed_header;
use super::frame::{assemble_frame, assemble_tiles};
use super::header::write_uncompressed_header;
use super::intermode::{write_inter_mode, write_is_inter, write_single_ref};
use super::mv::encode_mv;
use super::quantize::quantize;
use super::syntax::{write_intra_mode, write_partition, write_selected_tx_size, write_skip};
use super::tokens::{coef_cost, cost_bit, encode_coefs, tree_bit_cost};
use super::transform::forward_transform;
use crate::block::{
    kf_uv_mode_probs, kf_y_mode_probs, partition_plane_context, skip_context, subsize,
    tx_size_context, update_partition_context, ModeInfo, Mv, ALTREF_FRAME, BLOCK_8X8, GOLDEN_FRAME,
    INTRA_FRAME, INTRA_MODE_TREE, LAST_FRAME, NEARESTMV, NEWMV, NONE_FRAME, PARTITION_NONE,
    PARTITION_SPLIT, PARTITION_TREE, ZEROMV,
};
use crate::decode::INTER_MODE_TREE;
use crate::decode::{
    adapt_coef_probs, clamp_mv_umv, intra_inter_context, single_ref_p1, single_ref_p2, uv_tx_size,
    FrameContext, FrameCounts,
};
use crate::geom_tables::{MAX_TXSIZE, SIZE_GROUP};
use crate::inter::{predict_block, RefPlane};
use crate::loopfilter::loop_filter_frame;
use crate::mv::{find_mv_refs, get_mode_context, lower_mv_precision, NmvCounts};
use crate::predict::{build_intra_edges, predict};
use crate::prob_tables::{DEFAULT_COEF_PROBS, KF_PARTITION_PROBS};
use crate::quant::{ac_quant, dc_quant};
use crate::token::get_scan;
use crate::transform::{
    inverse_transform_add_rows, inverse_transform_dc_add, TxType, INTRA_MODE_TO_TX_TYPE,
};
use crate::FrameHeader;

const MI_SIZE: usize = 8;
const BLOCK_64X64: usize = 12;
// Intra prediction modes (ISO/VP9 enum order).
const DC_PRED: u8 = 0;
const V_PRED: u8 = 1;
const H_PRED: u8 = 2;
const TM_PRED: u8 = 9;

/// RDO trial snapshot of a luma block: `(pixels[row·bw+col] up to 64×64, above_ctx
/// footprint, whole left_ctx column)`.
type YSnap = ([u16; 4096], [u8; 16], [u8; 16]);

/// Full-block snapshot for the recursive partition RD: everything a block trial
/// mutates (all three reconstructed planes over the block region, the entropy
/// coefficient contexts, the partition segment contexts, and the mode-info grid),
/// so a NONE trial can be rolled back before trying SPLIT and vice-versa.
struct BlockSnap {
    rec: [Vec<u16>; 3],
    above_ctx: [Vec<u8>; 3],
    left_ctx: [[u8; 16]; 3],
    above_seg: Vec<u8>,
    left_seg: [u8; 8],
    mi: Vec<ModeInfo>,
    x_mis: usize,
    y_mis: usize,
}

/// One reconstructed/source plane (coded size: `mi_*·8 >> ss`).
struct Plane {
    buf: Vec<u16>,
    stride: usize,
    ss_x: usize,
    ss_y: usize,
    w: usize,
    h: usize,
}

/// Intra key-frame encoder. Coordinates are in the coded grid (rounded up to 8).
pub struct FrameEncoder {
    width: u32,
    height: u32,
    mi_rows: usize,
    mi_cols: usize,
    qindex: u32,
    src: [Plane; 3],
    rec: [Plane; 3],
    mi: Vec<ModeInfo>,
    above_seg: Vec<u8>,
    left_seg: [u8; 8],
    above_ctx: [Vec<u8>; 3],
    left_ctx: [[u8; 16]; 3],
    dq_y: (i32, i32),
    dq_uv: (i32, i32),
    max_px: i32,
    // Inter-frame state (no references ⇒ key frame). Slots are [LAST, GOLDEN,
    // ALTREF]; `active_ref` selects which one the motion search / MC currently read.
    is_inter: bool,
    refs: [Option<[Plane; 3]>; 3],
    active_ref: usize,
    /// Which reference slots this frame writes (bit i ⇒ slot i). Default `1` (refresh
    /// LAST only); a hidden ALT-REF frame sets bit 2 instead.
    refresh_frame_flags: u32,
    /// `show_frame`: a hidden ALT-REF (temporal future reference) is coded with
    /// `false` and displayed later via `show_existing_frame`.
    show_frame: bool,
    /// Physical ref-slot each logical reference (LAST/GOLDEN/ALTREF) reads from. The
    /// decoder does `active[i] = ref_frames[ref_frame_idx[i]]`; the encoder must match.
    ref_frame_idx: [usize; 3],
    interp_filter: u32,
    sign_bias: [bool; 4],
    fc: FrameContext,
    // RDO: when `use_rdo`, mode decisions minimise `SSE + lambda·bits` instead of
    // distortion alone; `lambda` is the rate-distortion multiplier from `qindex`.
    use_rdo: bool,
    lambda: f64,
    // The deblocking level chosen by the most recent `encode_frame` (R3).
    lf_level: u32,
    // R4 — forward coefficient-probability updates. `counts` accumulates the
    // committed token statistics; `commit_fc`, when set, holds the adapted
    // context the second pass codes (and the header signals) the tile with.
    counts: FrameCounts,
    commit_fc: Option<FrameContext>,
    use_prob_updates: bool,
    /// Debug only: force loop_filter_level 0 (isolates the loop filter in tests).
    disable_lf: bool,
    /// Running sum of the tx-block EOBs coded since the last reset — lets
    /// `encode_inter_block` detect a fully-empty block and code `skip` instead of
    /// empty coefficient tokens (which a conformant decoder mis-tracks).
    pending_eob: u32,
    /// When set, the trial (`encode_plane(None)`) applies the trellis just like the
    /// commit — so the skip decision sees the *post-trellis* EOB (the trellis can
    /// empty a block the raw quantizer didn't).
    skip_trial: bool,
    // R5 — AC deadzone: the AC rounding offset as `ac_step·ac_round_num/8`.
    // 4 = round-to-nearest; 3 rounds AC toward zero (RD-aware deadzone).
    ac_round_num: i64,
    // R5 — trellis-style RD-optimal end-of-block (drop trailing coefficients by RD).
    use_trellis: bool,
    // Roof — per-block transform-size search (4×4 vs 8×8 for 8×8 luma blocks).
    use_tx_search: bool,
    // Roof — partition control. `force_min_bsize` codes PARTITION_NONE once a block
    // reaches this size (default BLOCK_8X8 = the historical all-8×8). Larger values
    // bring up bigger blocks; `use_partition_rd` turns on the recursive RD search.
    force_min_bsize: usize,
    use_partition_rd: bool,
    /// Partition decision recorded by the RD pass, keyed by `(mi_row, mi_col,
    /// bsize)`; read by `encode_partition` during the emit pass(es).
    part_map: std::collections::HashMap<(usize, usize, usize), u8>,
}

impl FrameEncoder {
    /// Create an encoder for a `width`×`height` frame. `src` holds the three
    /// planes (Y full-res, U/V half-res for 4:2:0) at the **coded** size
    /// (`mi_*·8`), row-major, 8-bit values in `u16`.
    pub fn new(
        width: u32,
        height: u32,
        qindex: u32,
        src_planes: [Vec<u16>; 3],
        ref_recon: Option<[Vec<u16>; 3]>,
    ) -> FrameEncoder {
        let mi_cols = ((width + 7) >> 3) as usize;
        let mi_rows = ((height + 7) >> 3) as usize;
        let cw = mi_cols * MI_SIZE;
        let ch = mi_rows * MI_SIZE;
        // Pad the recon/source *height* up to whole superblocks so a bottom-edge NONE
        // block may overhang the frame: a tx block whose top is in-frame but whose
        // 32×32 extent spills below `ch` reconstructs into these padding rows (the
        // decoder does the same into its padded frame buffer). Stride stays the coded
        // width — horizontal overhang is avoided by the `full_fit` partition rule — so
        // `recon()` remains a plain crop of the leading `cw·ch` samples.
        let ch_pad = mi_rows.div_ceil(8) * 64;
        let mk = |ss_x: usize, ss_y: usize, buf: Vec<u16>| {
            let w = cw >> ss_x;
            let h = ch >> ss_y;
            Plane {
                buf,
                stride: w,
                ss_x,
                ss_y,
                w,
                h,
            }
        };
        // Copy an unpadded (`w×h`) source plane into a `w×hp` buffer, replicating the
        // last in-frame row into the vertical padding (libvpx `extend_frame`), so the
        // forward transform of an overhang tx block sees a valid (edge-extended) source.
        let pad_v = |p: Vec<u16>, w: usize, h: usize, hp: usize| -> Vec<u16> {
            let mut out = vec![0u16; w * hp];
            out[..w * h].copy_from_slice(&p[..w * h]);
            for y in h..hp {
                out.copy_within((h - 1) * w..h * w, y * w);
            }
            out
        };
        let [sy, su, sv] = src_planes;
        let src = [
            mk(0, 0, pad_v(sy, cw, ch, ch_pad)),
            mk(1, 1, pad_v(su, cw / 2, ch / 2, ch_pad / 2)),
            mk(1, 1, pad_v(sv, cw / 2, ch / 2, ch_pad / 2)),
        ];
        let rec = [
            mk(0, 0, vec![0u16; cw * ch_pad]),
            mk(1, 1, vec![0u16; (cw / 2) * (ch_pad / 2)]),
            mk(1, 1, vec![0u16; (cw / 2) * (ch_pad / 2)]),
        ];
        let is_inter = ref_recon.is_some();
        let ref_planes = ref_recon.map(|[ry, ru, rv]| [mk(0, 0, ry), mk(1, 1, ru), mk(1, 1, rv)]);
        let dc_y = dc_quant(qindex as i32, 8);
        let ac_y = ac_quant(qindex as i32, 8);
        FrameEncoder {
            width,
            height,
            mi_rows,
            mi_cols,
            qindex,
            src,
            rec,
            mi: vec![ModeInfo::default(); mi_rows * mi_cols],
            above_seg: vec![0u8; mi_cols],
            left_seg: [0u8; 8],
            above_ctx: [
                vec![0u8; mi_cols * 2],
                vec![0u8; mi_cols],
                vec![0u8; mi_cols],
            ],
            left_ctx: [[0u8; 16]; 3],
            // No segmentation / delta-q: one quantizer for Y and one for UV.
            dq_y: (dc_y, ac_y),
            dq_uv: (dc_quant(qindex as i32, 8), ac_quant(qindex as i32, 8)),
            max_px: 255,
            is_inter,
            refs: [ref_planes, None, None],
            active_ref: 0,
            refresh_frame_flags: 1, // refresh LAST (slot 0) by default
            show_frame: true,
            ref_frame_idx: [0, 1, 2],
            interp_filter: 0,      // EIGHTTAP, frame-level fixed (not switchable)
            sign_bias: [false; 4], // all same ⇒ no compound, reference_mode forced single
            fc: FrameContext::defaults(),
            use_rdo: true,
            // Rate-distortion multiplier `ac²·mult` for `J = SSE + lambda·bits`.
            // mult = 0.001 was calibrated via the BD-rate oracle (`lambda_calibration`):
            // it beats the original 0.02 guess by −2.2% BD-rate (which was ~20× too
            // high — the trellis exposed it as a +40% catastrophe).
            lambda: (ac_y as f64) * (ac_y as f64) * 0.001,
            lf_level: 0,
            counts: FrameCounts::zeroed(),
            commit_fc: None,
            use_prob_updates: true,
            // R5 AC deadzone: OFF (4 = round-to-nearest). It lowered the encoder's
            // own RD cost J ~1.6%, but the unbiased BD-rate oracle
            // (`encode::quality`) proved it's a **+1.66% LOSS** (sheds bitrate for
            // far more PSNR) — the J self-metric flattered a loss. Kept as the
            // worked example of why a video RD knob needs the BD-rate gate.
            ac_round_num: 4,
            // R5 — ON: BD-rate oracle scores it −0.45% at the calibrated λ (a real
            // win; the same knob was a +40% catastrophe at the old too-high λ).
            use_trellis: true,
            // Roof — ON: BD-rate oracle scores it −18% (an 8×8 transform decorrelates
            // smooth residual far better than four 4×4s — fewer bits AND higher PSNR).
            use_tx_search: true,
            force_min_bsize: BLOCK_8X8, // only used when partition RD is off
            // Roof — ON: BD-rate oracle scores recursive partitioning −37% vs all-8×8
            // (large blocks are far cheaper on smooth content; it can always fall back
            // to 8×8 on detail). Key-frame only for now — inter stays all-8×8.
            use_partition_rd: true,
            disable_lf: false,
            pending_eob: 0,
            skip_trial: false,
            part_map: std::collections::HashMap::new(),
        }
    }

    pub fn recon_owned(&self) -> [Vec<u16>; 3] {
        // Crop off the bottom-overhang padding rows (stride == coded width `w`).
        std::array::from_fn(|p| self.rec[p].buf[..self.rec[p].w * self.rec[p].h].to_vec())
    }

    /// Build reference planes (coded `cw×ch`, unpadded — MC clamps to the border) from
    /// a previous frame's `recon_owned()` at this frame's size.
    fn ref_planes_from(&self, recon: [Vec<u16>; 3]) -> [Plane; 3] {
        let (cw, ch) = (self.mi_cols * MI_SIZE, self.mi_rows * MI_SIZE);
        let mk = |ss_x: usize, ss_y: usize, buf: Vec<u16>| Plane {
            buf,
            stride: cw >> ss_x,
            ss_x,
            ss_y,
            w: cw >> ss_x,
            h: ch >> ss_y,
        };
        let [ry, ru, rv] = recon;
        [mk(0, 0, ry), mk(1, 1, ru), mk(1, 1, rv)]
    }

    /// Install the GOLDEN reference (slot 1) — a long-term frame (typically the last
    /// key frame) the per-block RD may choose instead of LAST. Same size as the frame.
    pub fn set_golden(&mut self, recon: [Vec<u16>; 3]) {
        self.refs[1] = Some(self.ref_planes_from(recon));
    }

    /// Install the ALTREF reference (slot 2) — a (usually hidden, future) frame.
    pub fn set_altref(&mut self, recon: [Vec<u16>; 3]) {
        self.refs[2] = Some(self.ref_planes_from(recon));
    }

    /// Mark this frame as a hidden ALT-REF: not shown (`show_frame = 0`) and refreshing
    /// only physical slot `slot`. It is displayed later via `show_existing_frame`.
    pub fn set_hidden_altref(&mut self, slot: usize) {
        self.show_frame = false;
        self.refresh_frame_flags = 1 << slot;
    }

    /// Override which physical ref slots LAST/GOLDEN/ALTREF read (for cross-GOP slot
    /// chaining); must match what the encoder installs via `new`/`set_golden`/`set_altref`.
    pub fn set_ref_frame_idx(&mut self, idx: [usize; 3]) {
        self.ref_frame_idx = idx;
    }

    /// Override the reference slots this frame refreshes (bit i ⇒ slot i).
    pub fn set_refresh_frame_flags(&mut self, flags: u32) {
        self.refresh_frame_flags = flags;
    }

    /// Per-mi chosen primary MV (1/8-pel) — for tests that check the motion search.
    #[cfg(test)]
    pub fn debug_block_mvs(&self) -> Vec<Mv> {
        self.mi.iter().map(|m| m.mv[0]).collect()
    }

    /// Per-mi `(is_inter, mode)` — for tests that check the intra-vs-inter choice.
    #[cfg(test)]
    pub fn debug_block_modes(&self) -> Vec<(bool, u8)> {
        self.mi.iter().map(|m| (m.is_inter, m.mode)).collect()
    }

    /// Per-mi primary reference frame (INTRA/LAST/GOLDEN/ALTREF) — for tests that
    /// check reference selection.
    #[cfg(test)]
    pub fn debug_block_refs(&self) -> Vec<i8> {
        self.mi.iter().map(|m| m.ref_frame[0]).collect()
    }

    /// Per-mi chosen tx size — for tests that check transform-size search.
    #[cfg(test)]
    pub fn debug_block_tx_sizes(&self) -> Vec<u8> {
        self.mi.iter().map(|m| m.tx_size).collect()
    }

    /// Number of mi cells coded with `skip` — for tests that check skip coding.
    #[cfg(test)]
    pub fn debug_skip_count(&self) -> usize {
        self.mi.iter().filter(|m| m.skip).count()
    }

    /// Per-mi block size (`sb_type`) — for tests that check partitioning.
    #[cfg(test)]
    pub fn debug_block_sizes(&self) -> Vec<u8> {
        self.mi.iter().map(|m| m.sb_type).collect()
    }

    /// Toggle rate-distortion optimisation (default on). With it off, mode
    /// decisions minimise distortion alone — the baseline the RD term improves on.
    #[cfg(test)]
    pub fn set_use_rdo(&mut self, v: bool) {
        self.use_rdo = v;
    }

    /// The deblocking level the last `encode_frame` chose (R3).
    #[cfg(test)]
    pub fn lf_level(&self) -> u32 {
        self.lf_level
    }

    /// Toggle R4 forward coefficient-probability updates (default on).
    #[cfg(test)]
    pub fn set_use_prob_updates(&mut self, v: bool) {
        self.use_prob_updates = v;
    }

    /// Set the AC deadzone numerator (R5): 4 = round-to-nearest, 3 = deadzone.
    #[cfg(test)]
    pub fn set_ac_round_num(&mut self, v: i64) {
        self.ac_round_num = v;
    }

    /// Toggle R5 trellis EOB optimization.
    #[cfg(test)]
    pub fn set_use_trellis(&mut self, v: bool) {
        self.use_trellis = v;
    }

    /// Toggle Roof per-block transform-size search.
    #[cfg(test)]
    pub fn set_use_tx_search(&mut self, v: bool) {
        self.use_tx_search = v;
    }

    /// Force PARTITION_NONE once a block reaches `bsize` (bring-up / partition control).
    #[cfg(test)]
    pub fn set_force_min_bsize(&mut self, bsize: usize) {
        self.force_min_bsize = bsize;
    }

    /// Toggle the recursive partition RD search.
    #[cfg(test)]
    pub fn set_use_partition_rd(&mut self, v: bool) {
        self.use_partition_rd = v;
    }

    #[cfg(test)]
    pub fn set_disable_lf(&mut self, v: bool) {
        self.disable_lf = v;
    }

    /// Set the RD multiplier as `ac_step²·mult` (calibration sweep). The shipped
    /// default mult is in `new`.
    #[cfg(test)]
    pub fn set_lambda_mult(&mut self, mult: f64) {
        self.lambda = (self.dq_y.1 as f64) * (self.dq_y.1 as f64) * mult;
    }

    /// The RD multiplier λ in `J = SSE + λ·bits`.
    #[cfg(test)]
    pub fn lambda(&self) -> f64 {
        self.lambda
    }

    /// Snapshot the luma reconstruction + entropy context a `bwl×bhl` block touches
    /// (up to 64×64), so an RDO trial (which reconstructs into them) can be rolled
    /// back. Luma is stored row-major with stride = block width.
    fn snap_y(&self, mi_row: usize, mi_col: usize, bwl: usize, bhl: usize) -> YSnap {
        let (x0, y0, bw, bh) = self.block_px(mi_row, mi_col, bwl, bhl, 0);
        let cw = self.rec[0].stride;
        let mut y = [0u16; 4096];
        for r in 0..bh {
            for c in 0..bw {
                y[r * bw + c] = self.rec[0].buf[(y0 + r) * cw + x0 + c];
            }
        }
        let mut above = [0u8; 16];
        let aw = bw / 4; // in-frame 4×4-columns the block spans
        above[..aw].copy_from_slice(&self.above_ctx[0][mi_col * 2..mi_col * 2 + aw]);
        (y, above, self.left_ctx[0])
    }

    fn restore_y(&mut self, mi_row: usize, mi_col: usize, bwl: usize, bhl: usize, snap: &YSnap) {
        let (x0, y0, bw, bh) = self.block_px(mi_row, mi_col, bwl, bhl, 0);
        let cw = self.rec[0].stride;
        for r in 0..bh {
            for c in 0..bw {
                self.rec[0].buf[(y0 + r) * cw + x0 + c] = snap.0[r * bw + c];
            }
        }
        let aw = bw / 4;
        self.above_ctx[0][mi_col * 2..mi_col * 2 + aw].copy_from_slice(&snap.1[..aw]);
        self.left_ctx[0] = snap.2;
    }

    /// Trial-code `mi`'s luma block and return its RD cost `SSE + lambda·bits`
    /// (distortion-only when `!use_rdo`), restoring the reconstruction + context to
    /// the pre-block state in `snap`. `extra_bits` accounts for mode-info bits the
    /// per-plane coder doesn't see (e.g. the NEWMV vector).
    #[allow(clippy::too_many_arguments)]
    fn rd_cost_y(
        &mut self,
        mi: &ModeInfo,
        mi_row: usize,
        mi_col: usize,
        bsize: usize,
        bwl: usize,
        bhl: usize,
        snap: &YSnap,
        extra_bits: f64,
    ) -> f64 {
        let (bits_q8, sse) = self.encode_plane(None, mi, 0, mi_row, mi_col, bsize, bwl, bhl);
        self.restore_y(mi_row, mi_col, bwl, bhl, snap);
        let rate = if self.use_rdo {
            self.lambda * (bits_q8 as f64 / 256.0 + extra_bits)
        } else {
            0.0
        };
        sse as f64 + rate
    }

    /// Emit the selected tx size using the prob array for this block's max tx
    /// (8×8 → `tx_p8x8`, 16×16 → `tx_p16x16`, 32×32 → `tx_p32x32`) — mirrors the
    /// decoder's `read_tx_size`.
    fn write_tx_size(&self, enc: &mut BoolEncoder, tx_size: u8, ctx: usize, max_tx: usize) {
        match max_tx {
            1 => write_selected_tx_size(enc, tx_size, &self.fc.tx_p8x8[ctx], max_tx),
            2 => write_selected_tx_size(enc, tx_size, &self.fc.tx_p16x16[ctx], max_tx),
            _ => write_selected_tx_size(enc, tx_size, &self.fc.tx_p32x32[ctx], max_tx),
        }
    }

    /// Roof — RD-pick the luma transform size (0..=`max_tx`) for `mi` (Roof).
    #[allow(clippy::too_many_arguments)]
    fn best_tx_size(
        &mut self,
        mi: &ModeInfo,
        mi_row: usize,
        mi_col: usize,
        bsize: usize,
        bwl: usize,
        bhl: usize,
        snap: &YSnap,
        max_tx: usize,
    ) -> u8 {
        let mut best = (0u8, f64::INFINITY);
        for t in 0..=max_tx as u8 {
            let mut m = *mi;
            m.tx_size = t;
            let j = self.rd_cost_y(&m, mi_row, mi_col, bsize, bwl, bhl, snap, 0.0);
            if j < best.1 {
                best = (t, j);
            }
        }
        best.0
    }

    /// The reconstructed planes (what the decoder must reproduce). Y, U, V. Cropped
    /// to the coded `w×h` region (dropping bottom-overhang padding rows).
    pub fn recon(&self) -> [&[u16]; 3] {
        [
            &self.rec[0].buf[..self.rec[0].w * self.rec[0].h],
            &self.rec[1].buf[..self.rec[1].w * self.rec[1].h],
            &self.rec[2].buf[..self.rec[2].w * self.rec[2].h],
        ]
    }

    fn above_mi(&self, mi_row: usize, mi_col: usize) -> Option<ModeInfo> {
        (mi_row > 0).then(|| self.mi[(mi_row - 1) * self.mi_cols + mi_col])
    }
    fn left_mi(&self, mi_row: usize, mi_col: usize) -> Option<ModeInfo> {
        (mi_col > 0).then(|| self.mi[mi_row * self.mi_cols + mi_col - 1])
    }

    /// Code the single tile over the whole frame (resetting the per-tile entropy
    /// context) and return its bytes. Driven twice by `encode_frame` for R4.
    fn encode_tile(&mut self) -> Vec<u8> {
        let mut enc = BoolEncoder::new();
        for c in self.above_ctx.iter_mut() {
            c.iter_mut().for_each(|v| *v = 0);
        }
        self.above_seg.iter_mut().for_each(|v| *v = 0);
        let mut mi_row = 0;
        while mi_row < self.mi_rows {
            self.left_seg = [0; 8];
            self.left_ctx = [[0; 16]; 3];
            let mut mi_col = 0;
            while mi_col < self.mi_cols {
                self.encode_partition(&mut enc, mi_row, mi_col, BLOCK_64X64, 4);
                mi_col += 8;
            }
            mi_row += 8;
        }
        enc.finish()
    }

    /// Encode the frame and return the complete VP9 bitstream.
    pub fn encode_frame(&mut self) -> Vec<u8> {
        let defaults = FrameContext::defaults();
        if self.partition_rd_active() {
            // Choose every partition by RD first; the emit pass(es) below replay
            // the recorded decisions. Reset the recon the decision pass left behind.
            self.run_partition_decision();
            for p in self.rec.iter_mut() {
                p.buf.iter_mut().for_each(|v| *v = 0);
            }
        }
        if self.use_prob_updates {
            // R4 pass 1: code with default probs to gather the committed token
            // counts, then forward-adapt the coefficient probs toward them.
            self.counts = FrameCounts::zeroed();
            self.commit_fc = None;
            let _ = self.encode_tile();
            let mut updated = FrameContext::defaults();
            adapt_coef_probs(
                &mut updated,
                &defaults,
                &self.counts,
                COEF_COUNT_SAT,
                COEF_MAX_UPDATE_FACTOR,
            );
            // RDO scores against the *default* probs, so pass 2 reproduces every
            // decision, level, and reconstructed pixel exactly — only the emitted
            // tokens (and the signalled probs) change. Reset the recon for it.
            for p in self.rec.iter_mut() {
                p.buf.iter_mut().for_each(|v| *v = 0);
            }
            self.commit_fc = Some(updated);
        } else {
            self.commit_fc = None;
        }
        // Final (or only) pass: code with the adapted probs if set, else defaults.
        let tile = self.encode_tile();
        let mut target = self.commit_fc.take().unwrap_or_else(FrameContext::defaults);
        if self.use_tx_search {
            target.tx_mode = 4; // TX_MODE_SELECT — per-block tx_size is coded
        }
        let tile_data = assemble_tiles(&[tile]);

        // ---- compressed header: signal the coef deltas; deblock the recon (R3) ----
        let mut h = self.frame_header();
        self.apply_loop_filter(&mut h);
        let mut cenc = BoolEncoder::new();
        write_compressed_header(&mut cenc, &defaults, &target, &h);
        let compressed = cenc.finish();

        // ---- uncompressed header (header_size now known) ----
        h.header_size = compressed.len() as u32;
        let mut w = BitWriter::new();
        write_uncompressed_header(&mut w, &h);
        let uncompressed = w.into_bytes();

        let mut frame = assemble_frame(&uncompressed, &compressed, &tile_data);
        // Guard the superframe framing: if the last byte aliases a superframe-index
        // marker (`b & 0xe0 == 0xc0`), a lenient external parser (e.g. ffmpeg) can
        // misread the whole frame as a superframe and fail. Append a padding byte —
        // the bool decoder ignores trailing bytes, so the frame still round-trips.
        if frame.last().is_some_and(|&b| b & 0xe0 == 0xc0) {
            frame.push(0);
        }
        frame
    }

    /// Emit a `show_existing_frame` packet re-displaying reference slot `idx` (0..7) —
    /// no new coded data. Used to display a previously-coded hidden ALT-REF at its
    /// place in display order.
    pub fn encode_show_existing_frame(idx: u32) -> Vec<u8> {
        let h = FrameHeader {
            show_existing_frame: true,
            frame_to_show: idx,
            ..Default::default()
        };
        let mut w = BitWriter::new();
        write_uncompressed_header(&mut w, &h);
        let mut frame = w.into_bytes();
        if frame.last().is_some_and(|&b| b & 0xe0 == 0xc0) {
            frame.push(0);
        }
        frame
    }

    fn frame_header(&self) -> FrameHeader {
        let mut h = FrameHeader {
            profile: 0,
            show_frame: self.show_frame,
            width: self.width,
            height: self.height,
            lossless: false, // qindex chosen > 0
            base_q_idx: self.qindex,
            loop_filter_level: 0, // chosen later by `apply_loop_filter` (R3)
            lf_ref_deltas: [1, 0, -1, -1],
            lf_mode_deltas: [0, 0],
            seg_tree_probs: [255; 7],
            seg_pred_probs: [255; 3],
            tile_cols_log2: 0,
            tile_rows_log2: 0,
            sized: true,
            ..Default::default()
        };
        if self.is_inter {
            // P frame: LAST=slot 0, GOLDEN=slot 1, ALTREF=slot 2, single ref, EIGHTTAP.
            // Slots 1/2 are written by the key frame (which refreshes all slots) and
            // persist; only slot 0 (LAST) is refreshed here. Blocks that reference an
            // absent GOLDEN/ALTREF simply never get chosen (the ref isn't installed).
            h.key_frame = false;
            h.refresh_frame_flags = self.refresh_frame_flags;
            h.ref_frame_idx = self.ref_frame_idx;
            h.ref_sign_bias = [false, false, false];
            h.allow_high_precision_mv = false;
            h.interp_filter = self.interp_filter;
            h.reset_frame_context = 0;
            // Error-resilient: each frame is independently decodable. This forces
            // `use_prev_frame_mvs = false` (we don't do temporal MV prediction — our
            // `find_mv_refs` passes `None`), disables backward adaptation (we only
            // forward-signal prob deltas), and codes every frame against the default
            // context. Without it a conformant decoder would use the previous P
            // frame's MVs as temporal candidates and diverge from frame 2 onward.
            h.error_resilient = true;
            // Color config is inherited (not coded) on inter frames → leave default.
        } else {
            h.key_frame = true;
            h.bit_depth = 8;
            h.color_space = 1; // CS_BT_601
            h.subsampling_x = 1;
            h.subsampling_y = 1;
        }
        h
    }

    /// Whether the recursive partition RD applies (key + inter — `rd_block_none`
    /// dispatches to the intra or inter decision).
    fn partition_rd_active(&self) -> bool {
        self.use_partition_rd
    }

    /// Mirror of `decode_partition` with the fixed all-8×8 decision.
    fn encode_partition(
        &mut self,
        enc: &mut BoolEncoder,
        mi_row: usize,
        mi_col: usize,
        bsize: usize,
        n4x4_l2: usize,
    ) {
        if mi_row >= self.mi_rows || mi_col >= self.mi_cols {
            return;
        }
        let n8x8_l2 = n4x4_l2 - 1;
        let num_8x8 = 1usize << n8x8_l2;
        let hbs = num_8x8 >> 1;
        let has_rows = mi_row + hbs < self.mi_rows;
        let has_cols = mi_col + hbs < self.mi_cols;
        let ctx = partition_plane_context(&self.above_seg, &self.left_seg, mi_row, mi_col, n8x8_l2);
        let probs = if self.is_inter {
            &self.fc.partition_prob[ctx]
        } else {
            &KF_PARTITION_PROBS[ctx]
        };

        // Partition decision: the recursive RD pass (if enabled) precomputed it into
        // `part_map`; otherwise code NONE once we reach `force_min_bsize` (and the
        // block fully fits — an edge block that doesn't fit must still split).
        let partition = if self.partition_rd_active() {
            self.part_map
                .get(&(mi_row, mi_col, bsize))
                .map(|&p| p as usize)
                .unwrap_or(PARTITION_SPLIT)
        } else {
            let force_none = bsize <= self.force_min_bsize && has_rows && has_cols;
            if hbs == 0 || force_none {
                PARTITION_NONE
            } else {
                PARTITION_SPLIT
            }
        };
        write_partition(enc, partition, probs, has_rows, has_cols);
        let subsize = subsize(partition, bsize) as usize;

        // NONE codes the whole block here; SPLIT recurses. (Gated on the partition,
        // NOT hbs — a forced-/RD-NONE at 16×16+ must not fall through to recursion.)
        if partition == PARTITION_NONE {
            self.encode_block(enc, mi_row, mi_col, subsize, n4x4_l2, n4x4_l2);
        } else {
            self.encode_partition(enc, mi_row, mi_col, subsize, n8x8_l2);
            self.encode_partition(enc, mi_row, mi_col + hbs, subsize, n8x8_l2);
            self.encode_partition(enc, mi_row + hbs, mi_col, subsize, n8x8_l2);
            self.encode_partition(enc, mi_row + hbs, mi_col + hbs, subsize, n8x8_l2);
        }
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
    }

    /// Mirror of `decode_block`: choose + write mode info, then reconstruct planes.
    fn encode_block(
        &mut self,
        enc: &mut BoolEncoder,
        mi_row: usize,
        mi_col: usize,
        bsize: usize,
        bwl: usize,
        bhl: usize,
    ) {
        if self.is_inter {
            self.encode_inter_block(enc, mi_row, mi_col, bsize, bwl, bhl);
            return;
        }
        let above = self.above_mi(mi_row, mi_col);
        let left = self.left_mi(mi_row, mi_col);
        // Decide mode + tx (the search); the bitstream codes tx_size *before* the
        // modes, so we decide everything then emit in the decoder's read order.
        let mi = self.decide_intra(mi_row, mi_col, bsize, bwl, bhl);

        let sctx = skip_context(above.as_ref(), left.as_ref());
        write_skip(enc, false, self.fc.skip_probs[sctx]);
        let max_tx = MAX_TXSIZE[bsize] as usize;
        if self.use_tx_search && max_tx >= 1 {
            let ctx = tx_size_context(&mi, above.as_ref(), left.as_ref());
            self.write_tx_size(enc, mi.tx_size, ctx, max_tx);
        }
        let yprobs = *kf_y_mode_probs(&mi, above.as_ref(), left.as_ref(), 0);
        write_intra_mode(enc, mi.mode, &yprobs);
        write_intra_mode(enc, mi.uv_mode, kf_uv_mode_probs(mi.mode));

        self.store_mi(mi_row, mi_col, bwl, bhl, &mi);
        for plane in 0..3 {
            self.encode_plane(Some(enc), &mi, plane, mi_row, mi_col, bsize, bwl, bhl);
        }
    }

    /// The intra mode + tx + uv-mode search for one key-frame block, factored out
    /// so the emit path (`encode_block`) and the partition-RD cost path
    /// (`rd_block_none`) make the *same* decision. Returns the chosen `ModeInfo`;
    /// leaves the reconstruction restored to the pre-block state.
    fn decide_intra(
        &mut self,
        mi_row: usize,
        mi_col: usize,
        bsize: usize,
        bwl: usize,
        bhl: usize,
    ) -> ModeInfo {
        let mut mi = ModeInfo {
            sb_type: bsize as u8,
            is_inter: false,
            skip: false,
            tx_size: 0,
            ..Default::default()
        };
        let snap = self.snap_y(mi_row, mi_col, bwl, bhl);
        let mut best = (DC_PRED, f64::MAX);
        for &m in &[DC_PRED, V_PRED, H_PRED, TM_PRED] {
            mi.mode = m;
            let j = self.rd_cost_y(&mi, mi_row, mi_col, bsize, bwl, bhl, &snap, 0.0);
            if j < best.1 {
                best = (m, j);
            }
        }
        mi.mode = best.0;
        let max_tx = MAX_TXSIZE[bsize] as usize;
        if self.use_tx_search && max_tx >= 1 {
            mi.tx_size = self.best_tx_size(&mi, mi_row, mi_col, bsize, bwl, bhl, &snap, max_tx);
        }
        mi.uv_mode = self.best_intra_mode(mi_row, mi_col, 1, bwl, bhl);
        mi
    }

    /// Store one block's mode info across the mi cells it covers.
    fn store_mi(&mut self, mi_row: usize, mi_col: usize, bwl: usize, bhl: usize, mi: &ModeInfo) {
        let x_mis = (1usize << (bwl - 1)).min(self.mi_cols - mi_col);
        let y_mis = (1usize << (bhl - 1)).min(self.mi_rows - mi_row);
        for y in 0..y_mis {
            for x in 0..x_mis {
                self.mi[(mi_row + y) * self.mi_cols + mi_col + x] = *mi;
            }
        }
    }

    // ---- recursive partition RD (Roof) -----------------------------------

    /// Q8 bit cost of one block's mode-info syntax (skip, tx_size, Y+UV modes) —
    /// the signaling the coefficient cost doesn't include, needed so the partition
    /// RD counts SPLIT's ~4× mode-info overhead against it.
    fn intra_modeinfo_cost_q8(&self, mi: &ModeInfo, mi_row: usize, mi_col: usize) -> u64 {
        let above = self.above_mi(mi_row, mi_col);
        let left = self.left_mi(mi_row, mi_col);
        let sctx = skip_context(above.as_ref(), left.as_ref());
        let mut c = cost_bit(self.fc.skip_probs[sctx], 0); // skip = false
        let bsize = mi.sb_type as usize;
        let max_tx = MAX_TXSIZE[bsize] as usize;
        if self.use_tx_search && max_tx >= 1 {
            let ctx = tx_size_context(mi, above.as_ref(), left.as_ref());
            c += self.tx_size_cost_q8(mi.tx_size, ctx, max_tx);
        }
        let yprobs = kf_y_mode_probs(mi, above.as_ref(), left.as_ref(), 0);
        c += tree_bit_cost(&INTRA_MODE_TREE, yprobs, mi.mode as i32);
        c += tree_bit_cost(
            &INTRA_MODE_TREE,
            kf_uv_mode_probs(mi.mode),
            mi.uv_mode as i32,
        );
        c
    }

    /// Q8 bit cost of the selected-tx-size tree (mirror of `write_selected_tx_size`).
    fn tx_size_cost_q8(&self, tx_size: u8, ctx: usize, max_tx: usize) -> u64 {
        let probs: &[u8] = match max_tx {
            1 => &self.fc.tx_p8x8[ctx],
            2 => &self.fc.tx_p16x16[ctx],
            _ => &self.fc.tx_p32x32[ctx],
        };
        let t = tx_size as usize;
        let mut c = cost_bit(probs[0], (t >= 1) as u32);
        if t >= 1 && max_tx >= 2 {
            c += cost_bit(probs[1], (t >= 2) as u32);
            if t >= 2 && max_tx >= 3 {
                c += cost_bit(probs[2], (t >= 3) as u32);
            }
        }
        c
    }

    /// Q8 bit cost of one *inter*-frame block's mode-info syntax (skip, is_inter,
    /// tx_size, ref + inter-mode + MV, or the intra-in-inter modes) — the partition-RD
    /// analogue of `intra_modeinfo_cost_q8`.
    fn inter_modeinfo_cost_q8(
        &self,
        mi: &ModeInfo,
        mi_row: usize,
        mi_col: usize,
        predictor: Mv,
    ) -> u64 {
        let above = self.above_mi(mi_row, mi_col);
        let left = self.left_mi(mi_row, mi_col);
        let bsize = mi.sb_type as usize;
        let sctx = skip_context(above.as_ref(), left.as_ref());
        let mut c = cost_bit(self.fc.skip_probs[sctx], mi.skip as u32);
        let ictx = intra_inter_context(above.as_ref(), left.as_ref());
        c += cost_bit(self.fc.intra_inter_prob[ictx], mi.is_inter as u32);
        let max_tx = MAX_TXSIZE[bsize] as usize;
        if self.use_tx_search && max_tx >= 1 && !mi.skip {
            let ctx = tx_size_context(mi, above.as_ref(), left.as_ref());
            c += self.tx_size_cost_q8(mi.tx_size, ctx, max_tx);
        }
        if mi.is_inter {
            // Single-ref selection: p1 (LAST vs {GOLDEN,ALTREF}) then, if not LAST, p2.
            let ctx0 = single_ref_p1(above.as_ref(), left.as_ref());
            let is_last = mi.ref_frame[0] == LAST_FRAME;
            c += cost_bit(self.fc.single_ref_prob[ctx0][0], (!is_last) as u32);
            if !is_last {
                let ctx1 = single_ref_p2(above.as_ref(), left.as_ref());
                c += cost_bit(
                    self.fc.single_ref_prob[ctx1][1],
                    (mi.ref_frame[0] == ALTREF_FRAME) as u32,
                );
            }
            let mctx = get_mode_context(
                &self.mi,
                self.mi_cols,
                self.mi_rows,
                0,
                self.mi_cols,
                mi_row,
                mi_col,
                bsize,
            );
            c += tree_bit_cost(
                &INTER_MODE_TREE,
                &self.fc.inter_mode_probs[mctx],
                (mi.mode - NEARESTMV) as i32,
            );
            if mi.mode == NEWMV {
                // Rough MV-delta cost (Q8): joint + per-component magnitude bits.
                let dr = (mi.mv[0].0 - predictor.0).unsigned_abs();
                let dc = (mi.mv[0].1 - predictor.1).unsigned_abs();
                let bits = 10 + 2 * ((32 - dr.leading_zeros()) + (32 - dc.leading_zeros()));
                c += bits as u64 * 256;
            }
        } else {
            c += tree_bit_cost(
                &INTRA_MODE_TREE,
                &self.fc.y_mode_prob[SIZE_GROUP[bsize] as usize],
                mi.mode as i32,
            );
            c += tree_bit_cost(
                &INTRA_MODE_TREE,
                &self.fc.uv_mode_prob[mi.mode as usize],
                mi.uv_mode as i32,
            );
        }
        c
    }

    /// RD cost of coding this block as a single (PARTITION_NONE) unit:
    /// `SSE + λ·(coef_bits + mode-info bits)`. Decides the modes, stores them,
    /// and leaves the block reconstructed into `rec` (so siblings predict from it).
    fn rd_block_none(
        &mut self,
        mi_row: usize,
        mi_col: usize,
        bsize: usize,
        bwl: usize,
        bhl: usize,
    ) -> f64 {
        if self.is_inter {
            // `decide_inter(keep_recon=true)` reconstructs + leaves the block in place.
            let (mi, predictor, coef_q8, sse) =
                self.decide_inter(mi_row, mi_col, bsize, bwl, bhl, true);
            self.store_mi(mi_row, mi_col, bwl, bhl, &mi);
            let bits_q8 = coef_q8 + self.inter_modeinfo_cost_q8(&mi, mi_row, mi_col, predictor);
            return sse as f64 + self.lambda * (bits_q8 as f64 / 256.0);
        }
        let mi = self.decide_intra(mi_row, mi_col, bsize, bwl, bhl);
        self.store_mi(mi_row, mi_col, bwl, bhl, &mi);
        let mut coef_q8 = 0u64;
        let mut sse = 0u64;
        for plane in 0..3 {
            let (b, s) = self.encode_plane(None, &mi, plane, mi_row, mi_col, bsize, bwl, bhl);
            coef_q8 += b;
            sse += s;
        }
        let bits_q8 = coef_q8 + self.intra_modeinfo_cost_q8(&mi, mi_row, mi_col);
        sse as f64 + self.lambda * (bits_q8 as f64 / 256.0)
    }

    /// λ-weighted cost of the partition flag itself at this node.
    fn part_flag_cost(
        &self,
        probs: &[u8; 3],
        partition: usize,
        has_rows: bool,
        has_cols: bool,
    ) -> f64 {
        let q8 = if has_rows && has_cols {
            tree_bit_cost(&PARTITION_TREE, probs, partition as i32)
        } else if !has_rows && has_cols {
            cost_bit(probs[1], (partition == PARTITION_SPLIT) as u32)
        } else if has_rows && !has_cols {
            cost_bit(probs[2], (partition == PARTITION_SPLIT) as u32)
        } else {
            0 // neither: SPLIT forced, no bits
        };
        self.lambda * (q8 as f64 / 256.0)
    }

    /// In-frame pixel extent of a block on plane `p` (clamped for partial edge SBs).
    fn block_px(
        &self,
        mi_row: usize,
        mi_col: usize,
        bwl: usize,
        bhl: usize,
        p: usize,
    ) -> (usize, usize, usize, usize) {
        let ss = (p != 0) as usize;
        let (x0, y0) = ((mi_col * 8) >> ss, (mi_row * 8) >> ss);
        let (cwp, chp) = ((self.mi_cols * 8) >> ss, (self.mi_rows * 8) >> ss);
        let bw = (((1usize << (bwl - 1)) * 8) >> ss).min(cwp - x0);
        let bh = (((1usize << (bhl - 1)) * 8) >> ss).min(chp - y0);
        (x0, y0, bw, bh)
    }

    fn snap_block(&self, mi_row: usize, mi_col: usize, bwl: usize, bhl: usize) -> BlockSnap {
        let rec = std::array::from_fn(|p| {
            let (x0, y0, bw, bh) = self.block_px(mi_row, mi_col, bwl, bhl, p);
            let st = self.rec[p].stride;
            let mut v = Vec::with_capacity(bw * bh);
            for r in 0..bh {
                v.extend_from_slice(&self.rec[p].buf[(y0 + r) * st + x0..(y0 + r) * st + x0 + bw]);
            }
            v
        });
        let x_mis = (1usize << (bwl - 1)).min(self.mi_cols - mi_col);
        let y_mis = (1usize << (bhl - 1)).min(self.mi_rows - mi_row);
        let mut mi = Vec::with_capacity(x_mis * y_mis);
        for y in 0..y_mis {
            let base = (mi_row + y) * self.mi_cols + mi_col;
            mi.extend_from_slice(&self.mi[base..base + x_mis]);
        }
        BlockSnap {
            rec,
            above_ctx: self.above_ctx.clone(),
            left_ctx: self.left_ctx,
            above_seg: self.above_seg.clone(),
            left_seg: self.left_seg,
            mi,
            x_mis,
            y_mis,
        }
    }

    fn restore_block(
        &mut self,
        mi_row: usize,
        mi_col: usize,
        bwl: usize,
        bhl: usize,
        s: &BlockSnap,
    ) {
        for p in 0..3 {
            let (x0, y0, bw, bh) = self.block_px(mi_row, mi_col, bwl, bhl, p);
            let st = self.rec[p].stride;
            for r in 0..bh {
                let src = &s.rec[p][r * bw..r * bw + bw];
                self.rec[p].buf[(y0 + r) * st + x0..(y0 + r) * st + x0 + bw].copy_from_slice(src);
            }
        }
        self.above_ctx = s.above_ctx.clone();
        self.left_ctx = s.left_ctx;
        self.above_seg = s.above_seg.clone();
        self.left_seg = s.left_seg;
        let mut k = 0;
        for y in 0..s.y_mis {
            for x in 0..s.x_mis {
                self.mi[(mi_row + y) * self.mi_cols + mi_col + x] = s.mi[k];
                k += 1;
            }
        }
    }

    /// Recursively choose the cheapest partition (NONE vs SPLIT) for the block by
    /// exact RD, mirroring `encode_partition`'s geometry. Records the decision in
    /// `part_map`, evolves the entropy/segment context as the winner would, and
    /// leaves the winner's reconstruction in `rec`. Returns the block's RD cost.
    fn rd_pick_partition(
        &mut self,
        mi_row: usize,
        mi_col: usize,
        bsize: usize,
        n4x4_l2: usize,
    ) -> f64 {
        // A quadrant entirely outside the frame contributes nothing (mirrors the
        // early return in `encode_partition`).
        if mi_row >= self.mi_rows || mi_col >= self.mi_cols {
            return 0.0;
        }
        let n8x8_l2 = n4x4_l2 - 1;
        let num_8x8 = 1usize << n8x8_l2;
        let hbs = num_8x8 >> 1;
        let has_rows = mi_row + hbs < self.mi_rows;
        let has_cols = mi_col + hbs < self.mi_cols;
        let ctx = partition_plane_context(&self.above_seg, &self.left_seg, mi_row, mi_col, n8x8_l2);
        let probs = if self.is_inter {
            self.fc.partition_prob[ctx]
        } else {
            KF_PARTITION_PROBS[ctx]
        };
        let can_split = hbs > 0;
        // NONE is eligible when the vertical half-point is in-frame (`has_rows`, so
        // `write_partition` can code NONE) AND the block fits horizontally in full.
        // A `has_rows` block may still overhang the *bottom* edge — its overhang tx
        // blocks reconstruct into the height padding (see `FrameEncoder::new`).
        // Horizontal overhang would need a padded stride, so those blocks still split
        // (they never satisfy the full-width test). `has_cols` is implied when the
        // block fits horizontally, so the coded partition tree is the full 4-way.
        let full_fit = has_rows && mi_col + num_8x8 <= self.mi_cols;

        let start = self.snap_block(mi_row, mi_col, n4x4_l2, n4x4_l2);

        // SPLIT — recurse into four quadrants (each leaves its own recon+context).
        let mut split_rd = f64::MAX;
        let mut split_snap = None;
        if can_split {
            let subsize = subsize(PARTITION_SPLIT, bsize) as usize;
            let mut s = self.part_flag_cost(&probs, PARTITION_SPLIT, has_rows, has_cols);
            s += self.rd_pick_partition(mi_row, mi_col, subsize, n8x8_l2);
            s += self.rd_pick_partition(mi_row, mi_col + hbs, subsize, n8x8_l2);
            s += self.rd_pick_partition(mi_row + hbs, mi_col, subsize, n8x8_l2);
            s += self.rd_pick_partition(mi_row + hbs, mi_col + hbs, subsize, n8x8_l2);
            split_rd = s;
            split_snap = Some(self.snap_block(mi_row, mi_col, n4x4_l2, n4x4_l2));
            self.restore_block(mi_row, mi_col, n4x4_l2, n4x4_l2, &start);
        }

        // NONE — code the whole block once (only when it fully fits the frame).
        let mut none_rd = f64::MAX;
        if full_fit {
            none_rd = self.part_flag_cost(&probs, PARTITION_NONE, has_rows, has_cols)
                + self.rd_block_none(mi_row, mi_col, bsize, n4x4_l2, n4x4_l2);
        }

        let choose_none = none_rd <= split_rd;
        let (partition, cost) = if choose_none {
            (PARTITION_NONE, none_rd) // NONE's recon+context already in place
        } else {
            self.restore_block(
                mi_row,
                mi_col,
                n4x4_l2,
                n4x4_l2,
                split_snap.as_ref().unwrap(),
            );
            (PARTITION_SPLIT, split_rd)
        };
        self.part_map
            .insert((mi_row, mi_col, bsize), partition as u8);

        // Evolve the segment (partition) context exactly as the emit pass will.
        let subsize = subsize(partition, bsize) as usize;
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
        cost
    }

    /// Fill `part_map` by running the recursive partition RD over every superblock,
    /// then leave `rec`/context ready to be reset for the emit pass.
    fn run_partition_decision(&mut self) {
        self.part_map.clear();
        for c in self.above_ctx.iter_mut() {
            c.iter_mut().for_each(|v| *v = 0);
        }
        self.above_seg.iter_mut().for_each(|v| *v = 0);
        let mut mi_row = 0;
        while mi_row < self.mi_rows {
            self.left_seg = [0; 8];
            self.left_ctx = [[0; 16]; 3];
            let mut mi_col = 0;
            while mi_col < self.mi_cols {
                self.rd_pick_partition(mi_row, mi_col, BLOCK_64X64, 4);
                mi_col += 8;
            }
            mi_row += 8;
        }
    }

    /// Mirror of `read_inter_frame_mode_info`: choose, per block, between inter
    /// (single ref LAST, ZEROMV or a searched-and-refined NEWMV) and intra
    /// (newly-revealed content the reference cannot predict). The chosen path's
    /// mode info, MC, and MV prediction all reuse the decoder's primitives, so
    /// the block round-trips bit-exact whichever way it goes.
    fn encode_inter_block(
        &mut self,
        enc: &mut BoolEncoder,
        mi_row: usize,
        mi_col: usize,
        bsize: usize,
        bwl: usize,
        bhl: usize,
    ) {
        let (mi, predictor, _, _) = self.decide_inter(mi_row, mi_col, bsize, bwl, bhl, false);
        let above = self.above_mi(mi_row, mi_col);
        let left = self.left_mi(mi_row, mi_col);
        let max_tx = MAX_TXSIZE[bsize] as usize;

        // segment_id = 0 (no bits). skip, is_inter, tx_size (not coded when skipped).
        let sctx = skip_context(above.as_ref(), left.as_ref());
        write_skip(enc, mi.skip, self.fc.skip_probs[sctx]);
        let ictx = intra_inter_context(above.as_ref(), left.as_ref());
        write_is_inter(enc, mi.is_inter, self.fc.intra_inter_prob[ictx]);
        if self.use_tx_search && max_tx >= 1 && !mi.skip {
            let ctx = tx_size_context(&mi, above.as_ref(), left.as_ref());
            self.write_tx_size(enc, mi.tx_size, ctx, max_tx);
        }
        if mi.is_inter {
            // Reference (single-prediction; compound disabled): p1 selects LAST vs
            // {GOLDEN,ALTREF} in context `single_ref_p1`; p2 selects between the latter
            // two in the *distinct* context `single_ref_p2` (mirrors `read_ref_frames`).
            let ctx0 = single_ref_p1(above.as_ref(), left.as_ref());
            let ctx1 = single_ref_p2(above.as_ref(), left.as_ref());
            write_single_ref(
                enc,
                mi.ref_frame[0],
                self.fc.single_ref_prob[ctx0][0],
                self.fc.single_ref_prob[ctx1][1],
            );
            let mctx = get_mode_context(
                &self.mi,
                self.mi_cols,
                self.mi_rows,
                0,
                self.mi_cols,
                mi_row,
                mi_col,
                bsize,
            );
            write_inter_mode(enc, mi.mode, &self.fc.inter_mode_probs[mctx]);
            if mi.mode == NEWMV {
                let mut counts = NmvCounts::default();
                encode_mv(enc, mi.mv[0], predictor, &self.fc.nmvc, false, &mut counts);
            }
        } else {
            // Intra inside an inter frame: Y mode by block-size group, then UV.
            write_intra_mode(
                enc,
                mi.mode,
                &self.fc.y_mode_prob[SIZE_GROUP[bsize] as usize],
            );
            write_intra_mode(enc, mi.uv_mode, &self.fc.uv_mode_prob[mi.mode as usize]);
        }

        self.store_mi(mi_row, mi_col, bwl, bhl, &mi);
        // Skipped blocks emit no coefficients; `decide_inter` already left the
        // motion-compensated prediction (and zeroed entropy context) in place.
        if !mi.skip {
            for plane in 0..3 {
                self.encode_plane(Some(enc), &mi, plane, mi_row, mi_col, bsize, bwl, bhl);
            }
        }
    }

    /// The per-block inter decision (mode + MV + tx + skip), factored out so the
    /// emit path (`encode_inter_block`) and the partition-RD cost path
    /// (`rd_block_none`) make the *same* choice. Reconstructs the block into `rec`
    /// (motion-compensated prediction, plus residual when not skipped) and returns
    /// `(mode_info, mv_predictor, coef_bits_q8, sse)`.
    fn decide_inter(
        &mut self,
        mi_row: usize,
        mi_col: usize,
        bsize: usize,
        bwl: usize,
        bhl: usize,
        keep_recon: bool,
    ) -> (ModeInfo, Mv, u64, u64) {
        // --- motion + mode search over every available reference (LAST/GOLDEN/ALTREF).
        // Each ref gets its own `find_mv_refs` predictor + motion search, then RD over
        // ZEROMV/NEWMV; the cheapest (ref, mode) across all refs wins (J = SSE+λ·bits). ---
        let edges = self.block_edges(mi_row, mi_col, bsize);
        let snap = self.snap_y(mi_row, mi_col, bwl, bhl);
        let ifilt = self.interp_filter as u8;
        let mk_inter = |rf: i8, mode: u8, mv: Mv| ModeInfo {
            sb_type: bsize as u8,
            skip: false,
            tx_size: 0,
            is_inter: true,
            ref_frame: [rf, NONE_FRAME],
            mode,
            mv: [mv, (0, 0)],
            interp_filter: ifilt,
            ..Default::default()
        };
        // (J, slot, ref_frame, mode, mv, predictor). LAST is always present on an inter
        // frame, so this is overwritten at least once.
        let mut best_inter: (f64, usize, i8, u8, Mv, Mv) =
            (f64::INFINITY, 0, LAST_FRAME, ZEROMV, (0, 0), (0, 0));
        for (slot, rf) in [(0usize, LAST_FRAME), (1, GOLDEN_FRAME), (2, ALTREF_FRAME)] {
            if self.refs[slot].is_none() {
                continue;
            }
            self.active_ref = slot;
            let (cand, _) = find_mv_refs(
                &self.mi,
                self.mi_cols,
                self.mi_rows,
                0,
                self.mi_cols,
                mi_row,
                mi_col,
                bsize,
                rf,
                &self.sign_bias,
                NEWMV,
                -1,
                edges,
                None,
            );
            let predictor = lower_mv_precision(cand[0], false);
            let best_mv = self.search_mv(mi_row, mi_col, predictor);
            // Rough ref-signalling cost so LAST (one bool) is preferred over GOLDEN/
            // ALTREF (two) on near-ties.
            let ref_bits = if rf == LAST_FRAME { 1.0 } else { 2.0 };
            let j_zero = self.rd_cost_y(
                &mk_inter(rf, ZEROMV, (0, 0)),
                mi_row,
                mi_col,
                bsize,
                bwl,
                bhl,
                &snap,
                4.0 + ref_bits,
            );
            if j_zero < best_inter.0 {
                best_inter = (j_zero, slot, rf, ZEROMV, (0, 0), predictor);
            }
            if best_mv != (0, 0) {
                let j_new = self.rd_cost_y(
                    &mk_inter(rf, NEWMV, best_mv),
                    mi_row,
                    mi_col,
                    bsize,
                    bwl,
                    bhl,
                    &snap,
                    16.0 + ref_bits,
                );
                if j_new < best_inter.0 {
                    best_inter = (j_new, slot, rf, NEWMV, best_mv, predictor);
                }
            }
        }

        // --- intra alternative (reference-independent) ---
        let mut intra_mi = ModeInfo {
            sb_type: bsize as u8,
            skip: false,
            tx_size: 0,
            is_inter: false,
            ref_frame: [INTRA_FRAME, NONE_FRAME],
            interp_filter: 3, // SWITCHABLE sentinel (matches the decoder)
            ..Default::default()
        };
        let mut best_intra = (DC_PRED, f64::INFINITY);
        for &m in &[DC_PRED, V_PRED, H_PRED, TM_PRED] {
            intra_mi.mode = m;
            let j = self.rd_cost_y(&intra_mi, mi_row, mi_col, bsize, bwl, bhl, &snap, 8.0);
            if j < best_intra.1 {
                best_intra = (m, j);
            }
        }

        let (best_j, best_slot, best_rf, best_mode, best_mv, predictor) = best_inter;
        let use_intra = best_intra.1 < best_j;
        // Lock the chosen reference in for the trial reconstruct + the emit MC.
        self.active_ref = best_slot;
        let mut chosen = if use_intra {
            let mut m = intra_mi;
            m.mode = best_intra.0;
            m
        } else {
            mk_inter(best_rf, best_mode, best_mv)
        };
        // UV mode for the intra path (before the trial, so its chroma cost is right).
        if use_intra {
            chosen.uv_mode = self.best_intra_mode(mi_row, mi_col, 1, bwl, bhl);
        }
        let max_tx = MAX_TXSIZE[bsize] as usize;
        if self.use_tx_search && max_tx >= 1 {
            chosen.tx_size =
                self.best_tx_size(&chosen, mi_row, mi_col, bsize, bwl, bhl, &snap, max_tx);
        }

        // Trial-reconstruct all planes to learn the total EOB (skip iff empty).
        // `skip_trial` makes it mirror the commit's trellis. A skipped block keeps its
        // (motion-compensated, zero-context) reconstruction; a non-skipped block rolls
        // back so the caller reconstructs from a clean neighbour-context state.
        let start = self.snap_block(mi_row, mi_col, bwl, bhl);
        let (mut coef_bits, mut sse) = (0u64, 0u64);
        self.pending_eob = 0;
        self.skip_trial = true;
        for plane in 0..3 {
            let (b, s) = self.encode_plane(None, &chosen, plane, mi_row, mi_col, bsize, bwl, bhl);
            coef_bits += b;
            sse += s;
        }
        self.skip_trial = false;
        let skip = !use_intra && self.pending_eob == 0;
        if skip {
            chosen.tx_size = if self.use_tx_search { max_tx as u8 } else { 0 };
            coef_bits = 0; // a skipped block codes no coefficient tokens
        } else if !keep_recon {
            // The emit path re-reconstructs from a clean neighbour-context state; the
            // RD path keeps this block's recon + context in place for its siblings.
            self.restore_block(mi_row, mi_col, bwl, bhl, &start);
        }
        chosen.skip = skip;
        (chosen, predictor, coef_bits, sse)
    }

    /// `mb_to_edges` (luma 1/8-pel border) for our only block size, 8×8 — where
    /// the libvpx `bw8`/`bh8` (block size in 8-pel units, halved) are both 1.
    fn block_edges(&self, mi_row: usize, mi_col: usize, _bsize: usize) -> (i32, i32, i32, i32) {
        let left = -((mi_col as i32 * 8) << 3);
        let right = (self.mi_cols as i32 - 1 - mi_col as i32) * 8 * 8;
        let top = -((mi_row as i32 * 8) << 3);
        let bottom = (self.mi_rows as i32 - 1 - mi_row as i32) * 8 * 8;
        (left, right, top, bottom)
    }

    /// Sum of absolute differences between the 8×8 source block at `(base_x,
    /// base_y)` and the reference shifted by integer pixels `(mv_r, mv_c)`,
    /// clamping reference reads to the plane border (as the MC convolver does).
    /// The currently-selected reference plane (LAST/GOLDEN/ALTREF per `active_ref`).
    #[inline]
    fn aref(&self, plane: usize) -> &Plane {
        &self.refs[self.active_ref].as_ref().unwrap()[plane]
    }

    fn block_sad(&self, base_x: usize, base_y: usize, mv_r: i32, mv_c: i32) -> i64 {
        let src = &self.src[0];
        let rp = self.aref(0);
        let (rw, rh) = (rp.w as i32, rp.h as i32);
        let mut sad = 0i64;
        for y in 0..8i32 {
            for x in 0..8i32 {
                let sx = (base_x as i32 + x + mv_c).clamp(0, rw - 1) as usize;
                let sy = (base_y as i32 + y + mv_r).clamp(0, rh - 1) as usize;
                let r = rp.buf[sy * rp.stride + sx] as i64;
                let s = src.buf[(base_y + y as usize) * src.stride + base_x + x as usize] as i64;
                sad += (s - r).abs();
            }
        }
        sad
    }

    /// SAD between the source luma block and the *actual* motion-compensated
    /// prediction for `mv` (1/8-pel) — runs the same `clamp_mv_umv` +
    /// `predict_block` (8-tap subpel) the decoder will, into a scratch buffer.
    fn predicted_sad(&self, mi_row: usize, mi_col: usize, mv: Mv) -> i64 {
        let base_x = mi_col * 8;
        let base_y = mi_row * 8;
        let edges = self.block_edges(mi_row, mi_col, BLOCK_8X8);
        let mv_q4 = clamp_mv_umv(mv, 8, 8, 0, 0, edges);
        let bx = base_x as i32 + (mv_q4.1 >> 4);
        let by = base_y as i32 + (mv_q4.0 >> 4);
        let rp = self.aref(0);
        let refp = RefPlane {
            buf: &rp.buf,
            stride: rp.stride,
            w: rp.w as i32,
            h: rp.h as i32,
        };
        let mut pred = [0u16; 64];
        predict_block(
            &refp,
            bx,
            by,
            (mv_q4.1 & 15) as usize,
            (mv_q4.0 & 15) as usize,
            self.interp_filter as usize,
            &mut pred,
            8,
            8,
            8,
            false,
            self.max_px,
        );
        let src = &self.src[0];
        let mut sad = 0i64;
        for y in 0..8 {
            for x in 0..8 {
                let s = src.buf[(base_y + y) * src.stride + base_x + x] as i64;
                sad += (s - pred[y * 8 + x] as i64).abs();
            }
        }
        sad
    }

    /// Motion search on the luma block. First a full ±8-pixel integer window
    /// (around the zero MV and the predictor), then a 1/4-pel refinement around
    /// the integer best scored against the true 8-tap prediction. Ties break
    /// toward the shorter MV (fewer coded bits). Returns the MV in 1/8-pel.
    /// Both whole-pixel and 1/4-pel MVs keep the difference vs the (even)
    /// predictor even, so the `!allow_high_precision_mv` "hp = 1" invariant holds.
    fn search_mv(&self, mi_row: usize, mi_col: usize, predictor: Mv) -> Mv {
        let base_x = mi_col * 8;
        let base_y = mi_row * 8;
        const RANGE: i32 = 8;
        let centers = [(0i32, 0i32), (predictor.0 / 8, predictor.1 / 8)];
        let mut best_px = (0i32, 0i32);
        let mut best_sad = i64::MAX;
        for &(cr, cc) in &centers {
            for dr in -RANGE..=RANGE {
                for dc in -RANGE..=RANGE {
                    let (r, c) = (cr + dr, cc + dc);
                    let sad = self.block_sad(base_x, base_y, r, c);
                    let shorter = r.abs() + c.abs() < best_px.0.abs() + best_px.1.abs();
                    if sad < best_sad || (sad == best_sad && shorter) {
                        best_sad = sad;
                        best_px = (r, c);
                    }
                }
            }
        }
        // 1/4-pel refinement (even 1/8-pel offsets ⇒ delta stays even). The integer
        // best lies within ±1/2 pel of the true minimum, so ±4 covers it.
        let int = (best_px.0 * 8, best_px.1 * 8);
        let mut best = int;
        let mut best_sad = self.predicted_sad(mi_row, mi_col, int);
        for dr in [-4i32, -2, 0, 2, 4] {
            for dc in [-4i32, -2, 0, 2, 4] {
                if dr == 0 && dc == 0 {
                    continue;
                }
                let cand = (int.0 + dr, int.1 + dc);
                let sad = self.predicted_sad(mi_row, mi_col, cand);
                let shorter = cand.0.abs() + cand.1.abs() < best.0.abs() + best.1.abs();
                if sad < best_sad || (sad == best_sad && shorter) {
                    best_sad = sad;
                    best = cand;
                }
            }
        }
        best
    }

    /// Motion-compensate the whole coding block for `mv` (1/8-pel) into the recon
    /// buffer — the exact non-scaled `mc_one` path (`clamp_mv_umv` + `predict_block`).
    #[allow(clippy::too_many_arguments)]
    fn inter_predict_mv(
        &mut self,
        plane: usize,
        mi_row: usize,
        mi_col: usize,
        bsize: usize,
        bwl: usize,
        bhl: usize,
        mv: Mv,
    ) {
        let (ss_x, ss_y) = (self.rec[plane].ss_x, self.rec[plane].ss_y);
        let base_x = (mi_col * MI_SIZE) >> ss_x;
        let base_y = (mi_row * MI_SIZE) >> ss_y;
        let n4_w = (1usize << bwl) >> ss_x;
        let n4_h = (1usize << bhl) >> ss_y;
        let (w, h) = (n4_w * 4, n4_h * 4);
        let edges = self.block_edges(mi_row, mi_col, bsize);
        let mv_q4 = clamp_mv_umv(mv, w as i32, h as i32, ss_x, ss_y, edges);
        let bx = base_x as i32 + (mv_q4.1 >> 4);
        let by = base_y as i32 + (mv_q4.0 >> 4);
        let subpel_x = (mv_q4.1 & 15) as usize;
        let subpel_y = (mv_q4.0 & 15) as usize;
        let stride = self.rec[plane].stride;
        let dst_off = base_y * stride + base_x;
        // Field-level borrow (disjoint from `self.rec` below) — a method borrowing all
        // of `&self` would conflict with the `&mut self.rec` destination.
        let rp = &self.refs[self.active_ref].as_ref().unwrap()[plane];
        let refp = RefPlane {
            buf: &rp.buf,
            stride: rp.stride,
            w: rp.w as i32,
            h: rp.h as i32,
        };
        predict_block(
            &refp,
            bx,
            by,
            subpel_x,
            subpel_y,
            self.interp_filter as usize,
            &mut self.rec[plane].buf[dst_off..],
            stride,
            w,
            h,
            false,
            self.max_px,
        );
    }

    /// Pick the cheapest intra mode (SAD of source vs prediction) for a plane —
    /// evaluated once on the top-left transform block as a cheap proxy.
    fn best_intra_mode(
        &self,
        mi_row: usize,
        mi_col: usize,
        plane: usize,
        bwl: usize,
        bhl: usize,
    ) -> u8 {
        let p = &self.src[plane];
        let r = &self.rec[plane];
        let base_x = (mi_col * MI_SIZE) >> p.ss_x;
        let base_y = (mi_row * MI_SIZE) >> p.ss_y;
        let fw = ((self.mi_cols * 8) >> p.ss_x) as i32;
        let fh = ((self.mi_rows * 8) >> p.ss_y) as i32;
        let bw_mi = 1usize << (bwl - 1);
        let bh_mi = 1usize << (bhl - 1);
        let mb_to_right =
            (self.mi_cols as i32 - bw_mi as i32 - mi_col as i32) * (MI_SIZE as i32) * 8;
        let mb_to_bottom =
            (self.mi_rows as i32 - bh_mi as i32 - mi_row as i32) * (MI_SIZE as i32) * 8;
        let up_avail = mi_row > 0;
        let left_avail = mi_col > 0;
        let bs = 4usize; // 4×4 tx-block proxy
        let n4_w = (1usize << bwl) >> p.ss_x;
        let right_avail = n4_w > 1;
        let mut best = DC_PRED;
        let mut best_sad = i64::MAX;
        let mut pred = vec![0u16; bs * bs];
        for &mode in &[DC_PRED, V_PRED, H_PRED, TM_PRED] {
            let mut above_buf = [0u16; 1 + 64];
            let mut left_buf = [0u16; 32];
            build_intra_edges(
                mode,
                bs,
                up_avail,
                left_avail,
                right_avail,
                &r.buf,
                r.stride,
                fw,
                fh,
                base_x as i32,
                base_y as i32,
                mb_to_right,
                mb_to_bottom,
                &mut above_buf,
                &mut left_buf,
                self.max_px,
            );
            predict(
                &mut pred,
                bs,
                mode,
                bs,
                &above_buf,
                &left_buf,
                left_avail,
                up_avail,
                self.max_px,
            );
            let mut sad = 0i64;
            for y in 0..bs {
                for x in 0..bs {
                    let s = p.buf[(base_y + y) * p.stride + base_x + x] as i64;
                    sad += (s - pred[y * bs + x] as i64).abs();
                }
            }
            if sad < best_sad {
                best_sad = sad;
                best = mode;
            }
        }
        best
    }

    /// Mirror of `reconstruct_plane`: iterate the transform-block grid. `enc =
    /// Some` commits; `enc = None` costs the plane for RDO. Returns `(bit cost in
    /// Q8, reconstruction SSE)` summed over the plane's transform blocks.
    #[allow(clippy::too_many_arguments)]
    fn encode_plane(
        &mut self,
        mut enc: Option<&mut BoolEncoder>,
        mi: &ModeInfo,
        plane: usize,
        mi_row: usize,
        mi_col: usize,
        bsize: usize,
        bwl: usize,
        bhl: usize,
    ) -> (u64, u64) {
        let (ss_x, ss_y) = (self.rec[plane].ss_x, self.rec[plane].ss_y);
        let n4_w = (1usize << bwl) >> ss_x;
        let n4_h = (1usize << bhl) >> ss_y;
        let tx_size = if plane == 0 {
            mi.tx_size as usize
        } else {
            uv_tx_size(bsize, mi.tx_size as usize, ss_x, ss_y)
        };
        let step = 1usize << tx_size;
        let bw_mi = 1usize << (bwl - 1);
        let bh_mi = 1usize << (bhl - 1);
        let mb_to_right =
            (self.mi_cols as i32 - bw_mi as i32 - mi_col as i32) * (MI_SIZE as i32) * 8;
        let mb_to_bottom =
            (self.mi_rows as i32 - bh_mi as i32 - mi_row as i32) * (MI_SIZE as i32) * 8;
        let max_w = if mb_to_right >= 0 {
            n4_w
        } else {
            (n4_w as i32 + (mb_to_right >> (5 + ss_x))).max(0) as usize
        };
        let max_h = if mb_to_bottom >= 0 {
            n4_h
        } else {
            (n4_h as i32 + (mb_to_bottom >> (5 + ss_y))).max(0) as usize
        };
        let above_some = self.above_mi(mi_row, mi_col).is_some();
        let left_some = self.left_mi(mi_row, mi_col).is_some();
        let base_x = (mi_col * MI_SIZE) >> ss_x;
        let base_y = (mi_row * MI_SIZE) >> ss_y;
        let above_col0 = (mi_col * 2) >> ss_x;
        let left_row0 = ((mi_row * 2) & 15) >> ss_y;

        // Inter blocks: motion-compensate the whole coding block first; the
        // per-tx-block loop then only adds the residual. Intra blocks (key frame,
        // or an intra fallback inside a P frame) predict per tx block instead.
        if mi.is_inter {
            self.inter_predict_mv(plane, mi_row, mi_col, bsize, bwl, bhl, mi.mv[0]);
        }

        let mut bits = 0u64;
        let mut sse = 0u64;
        let mut row = 0;
        while row < max_h {
            let mut col = 0;
            while col < max_w {
                let (b, s) = self.encode_tx_block(
                    enc.as_deref_mut(),
                    mi,
                    plane,
                    tx_size,
                    n4_w,
                    row,
                    col,
                    base_x,
                    base_y,
                    above_col0,
                    left_row0,
                    above_some,
                    left_some,
                    max_w,
                    max_h,
                    mb_to_right,
                    mb_to_bottom,
                );
                bits += b;
                sse += s;
                col += step;
            }
            row += step;
        }
        (bits, sse)
    }

    /// Mirror of `reconstruct_tx_block`, forward direction. With `enc = Some` it
    /// *commits* (emits tokens); with `enc = None` it *costs* the block instead
    /// (RDO trial) — both reconstruct identically. Returns `(bit cost in Q8,
    /// reconstruction SSE)`; the bit cost is 0 on the commit path (already spent).
    #[allow(clippy::too_many_arguments)]
    fn encode_tx_block(
        &mut self,
        enc: Option<&mut BoolEncoder>,
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
    ) -> (u64, u64) {
        let txw = 1usize << tx_size;
        let bs = 4usize << tx_size;
        let stride = self.rec[plane].stride;
        let fw = ((self.mi_cols * 8) >> self.rec[plane].ss_x) as i32;
        let fh = ((self.mi_rows * 8) >> self.rec[plane].ss_y) as i32;
        let x0 = base_x + col * 4;
        let y0 = base_y + row * 4;
        let dst_off = y0 * stride + x0;

        // ---- intra prediction into the recon buffer (inter blocks were already
        // motion-compensated by `inter_predict`) ----
        if !mi.is_inter {
            let mode = if plane == 0 { mi.mode } else { mi.uv_mode };
            let up_avail = row > 0 || above_some;
            let left_avail = col > 0 || left_some;
            let right_avail = (col + txw) < n4_w;
            let mut above_buf = [0u16; 1 + 64];
            let mut left_buf = [0u16; 32];
            build_intra_edges(
                mode,
                bs,
                up_avail,
                left_avail,
                right_avail,
                &self.rec[plane].buf,
                stride,
                fw,
                fh,
                x0 as i32,
                y0 as i32,
                mb_to_right,
                mb_to_bottom,
                &mut above_buf,
                &mut left_buf,
                self.max_px,
            );
            predict(
                &mut self.rec[plane].buf[dst_off..],
                stride,
                mode,
                bs,
                &above_buf,
                &left_buf,
                left_avail,
                up_avail,
                self.max_px,
            );
        }

        // Reused stack scratch (max 32×32 = 1024) — no per-block heap allocation.
        let n = bs * bs;
        let mut residual = [0i32; 1024];
        let src = &self.src[plane];
        for y in 0..bs {
            for x in 0..bs {
                let s = src.buf[(y0 + y) * src.stride + x0 + x] as i32;
                let p = self.rec[plane].buf[dst_off + y * stride + x] as i32;
                residual[y * bs + x] = s - p;
            }
        }

        // ---- forward transform + quantize (inter / lossless / chroma / 32×32
        // are always DCT_DCT; only ≤16×16 intra luma uses the hybrid transform) ----
        let tx_type = if mi.is_inter || self.frame_lossless() || plane != 0 || tx_size == 3 {
            TxType::DctDct
        } else {
            INTRA_MODE_TO_TX_TYPE[mi.mode as usize]
        };
        let (scan, nb) = get_scan(tx_size, tx_type);
        let dq = if plane == 0 { self.dq_y } else { self.dq_uv };
        let dq_shift = if tx_size == 3 { 1 } else { 0 };
        let mut coeffs = [0i32; 1024];
        forward_transform(&residual[..n], bs, tx_type, &mut coeffs[..n]);
        let mut levels = [0i32; 1024];
        let mut dqcoeff = [0i32; 1024];
        let mut eob = quantize(
            &coeffs[..n],
            scan,
            dq.0,
            dq.1,
            dq.1 as i64 * self.ac_round_num / 8,
            dq_shift,
            &mut levels[..n],
            &mut dqcoeff[..n],
        );

        // ---- entropy context, then encode the tokens ----
        let act = self.above_ctx[plane][above_col0 + col..above_col0 + col + txw]
            .iter()
            .any(|&v| v != 0) as usize;
        let lct = self.left_ctx[plane][left_row0 + row..left_row0 + row + txw]
            .iter()
            .any(|&v| v != 0) as usize;
        let ctx0 = act + lct;
        let pt = plane.min(1);
        let inter = mi.is_inter as usize;
        let default_probs = &DEFAULT_COEF_PROBS[tx_size][pt][inter];
        // R5: trellis-style RD-optimal EOB on the commit path (uses the default
        // probs so the levels reproduce identically across the R4 two-pass).
        if (enc.is_some() || self.skip_trial) && self.use_trellis && eob > 0 {
            eob = self.trellis_eob(
                &mut levels[..n],
                &mut dqcoeff[..n],
                scan,
                nb,
                eob,
                ctx0,
                default_probs,
                tx_size,
                tx_type,
                bs,
                x0,
                y0,
                dst_off,
                stride,
                plane,
            );
        }
        let mut token_cache = [0u8; 1024];
        let bits = if let Some(enc) = enc {
            // Commit: code with the adapted probs in pass 2 (R4), else the
            // defaults, and tally the token counts for the forward update.
            let probs = self
                .commit_fc
                .as_ref()
                .map(|fc| &fc.coef_probs[tx_size][pt][inter])
                .unwrap_or(default_probs);
            let mut coef_cnt = [[[0u32; 4]; 6]; 6];
            let mut eob_cnt = [[0u32; 6]; 6];
            encode_coefs(
                enc,
                &levels[..n],
                scan,
                nb,
                eob,
                probs,
                tx_size,
                ctx0,
                &mut token_cache[..n],
                &mut coef_cnt,
                &mut eob_cnt,
                8,
            );
            let cc = &mut self.counts.coef[tx_size][pt][inter];
            let ec = &mut self.counts.eob_branch[tx_size][pt][inter];
            for band in 0..6 {
                for c in 0..6 {
                    for m in 0..4 {
                        cc[band][c][m] += coef_cnt[band][c][m];
                    }
                    ec[band][c] += eob_cnt[band][c];
                }
            }
            0
        } else {
            // RDO trial: cost the exact same token walk (default probs) without emitting.
            coef_cost(
                &levels[..n],
                scan,
                nb,
                eob,
                default_probs,
                tx_size,
                ctx0,
                &mut token_cache[..n],
                8,
            )
        };

        // ---- update entropy context (libvpx ctx_shift) ----
        let inframe_w = (max_w - col).min(txw);
        let inframe_h = (max_h - row).min(txw);
        let v = (eob > 0) as u8;
        for i in 0..txw {
            self.above_ctx[plane][above_col0 + col + i] = if i < inframe_w { v } else { 0 };
            self.left_ctx[plane][left_row0 + row + i] = if i < inframe_h { v } else { 0 };
        }
        self.pending_eob += eob as u32;

        // ---- reconstruct: add the dequantized residual back ----
        if eob > 0 {
            let dst = &mut self.rec[plane].buf[dst_off..];
            if eob == 1 && tx_type == TxType::DctDct {
                inverse_transform_dc_add(dqcoeff[0], bs, dst, stride, self.max_px);
            } else {
                // max_row = highest non-zero coefficient row.
                let mut max_row = 0usize;
                for (pos, &c) in dqcoeff[..n].iter().enumerate() {
                    if c != 0 {
                        max_row = max_row.max(pos / bs);
                    }
                }
                inverse_transform_add_rows(
                    &dqcoeff[..n],
                    bs,
                    tx_type,
                    dst,
                    stride,
                    self.max_px,
                    max_row + 1,
                );
            }
        }

        // ---- distortion: SSE of the reconstruction vs the source (for RDO) ----
        let src = &self.src[plane];
        let rec = &self.rec[plane].buf;
        let mut sse = 0u64;
        for y in 0..bs {
            for x in 0..bs {
                let s = src.buf[(y0 + y) * src.stride + x0 + x] as i64;
                let r = rec[dst_off + y * stride + x] as i64;
                let d = s - r;
                sse += (d * d) as u64;
            }
        }
        (bits, sse)
    }

    /// R5 trellis EOB: greedily drop trailing non-zero coefficients while doing so
    /// lowers the *exact* RD cost `J = SSE + λ·bits` (real pixel distortion from a
    /// real inverse transform, real token cost from `coef_cost`). Returns the new
    /// EOB; `levels`/`dqcoeff` are zeroed past it. Bit-exact: the decoder simply
    /// reconstructs whatever coefficients survive.
    #[allow(clippy::too_many_arguments)]
    fn trellis_eob(
        &self,
        levels: &mut [i32],
        dqcoeff: &mut [i32],
        scan: &[i16],
        nb: &[i16],
        eob: usize,
        ctx0: usize,
        probs: &[[[u8; 3]; 6]; 6],
        tx_size: usize,
        tx_type: TxType,
        bs: usize,
        x0: usize,
        y0: usize,
        dst_off: usize,
        stride: usize,
        plane: usize,
    ) -> usize {
        let n = bs * bs;
        // The prediction still sits in the recon buffer (residual added later).
        let mut pred = [0u16; 1024];
        for y in 0..bs {
            for x in 0..bs {
                pred[y * bs + x] = self.rec[plane].buf[dst_off + y * stride + x];
            }
        }
        let src = &self.src[plane];
        let rd = |dq: &[i32], lv: &[i32], e: usize| -> f64 {
            let mut temp = [0u16; 1024];
            temp[..n].copy_from_slice(&pred[..n]);
            if e > 0 {
                let mut max_row = 0;
                for (p, &c) in dq[..n].iter().enumerate() {
                    if c != 0 {
                        max_row = max_row.max(p / bs);
                    }
                }
                inverse_transform_add_rows(
                    &dq[..n],
                    bs,
                    tx_type,
                    &mut temp[..n],
                    bs,
                    self.max_px,
                    max_row + 1,
                );
            }
            let mut d = 0u64;
            for y in 0..bs {
                for x in 0..bs {
                    let s = src.buf[(y0 + y) * src.stride + x0 + x] as i64;
                    let r = temp[y * bs + x] as i64;
                    d += ((s - r) * (s - r)) as u64;
                }
            }
            let mut tc = [0u8; 1024];
            let r = coef_cost(&lv[..n], scan, nb, e, probs, tx_size, ctx0, &mut tc[..n], 8);
            d as f64 + self.lambda * (r as f64 / 256.0)
        };
        let mut eob = eob;
        let mut j = rd(dqcoeff, levels, eob);
        while eob > 0 {
            let last = scan[eob - 1] as usize;
            let (sl, sd) = (levels[last], dqcoeff[last]);
            levels[last] = 0;
            dqcoeff[last] = 0;
            let mut ne = eob - 1;
            while ne > 0 && levels[scan[ne - 1] as usize] == 0 {
                ne -= 1;
            }
            let jp = rd(dqcoeff, levels, ne);
            if jp < j {
                j = jp;
                eob = ne;
            } else {
                levels[last] = sl;
                dqcoeff[last] = sd;
                break;
            }
        }
        eob
    }

    fn frame_lossless(&self) -> bool {
        self.qindex == 0
    }

    /// Luma reconstruction SSE vs the source for an alternative luma buffer.
    fn luma_sse_of(&self, buf: &[u16]) -> u64 {
        let src = &self.src[0].buf;
        let n = self.src[0].w * self.src[0].h; // coded region only (skip padding rows)
        let mut sse = 0u64;
        for i in 0..n {
            let d = src[i] as i64 - buf[i] as i64;
            sse += (d * d) as u64;
        }
        sse
    }

    /// R3 — loop-filter-level search: pick the `loop_filter_level` whose deblocked
    /// reconstruction is closest to the source (luma SSE), set it in the header,
    /// and apply that filter to the reconstruction (a uniform filter —
    /// `lf_delta_enabled = false`). The decoder reads the level and reproduces the
    /// exact same deblocked frame, so the round-trip stays bit-exact.
    fn apply_loop_filter(&mut self, h: &mut FrameHeader) {
        if self.disable_lf {
            h.loop_filter_level = 0;
            self.lf_level = 0;
            return;
        }
        // Level 0 (no filter) is the baseline; coarse candidates cover the useful
        // range (high levels rarely beat moderate ones for SSE).
        let mut best = (0u32, self.luma_sse_of(&self.rec[0].buf));
        for &lvl in &[8u32, 16, 24, 32] {
            h.loop_filter_level = lvl;
            let mut c0 = self.rec[0].buf.clone();
            let mut c1 = self.rec[1].buf.clone();
            let mut c2 = self.rec[2].buf.clone();
            let mut planes = [
                (&mut c0[..], self.rec[0].stride, 0usize, 0usize),
                (&mut c1[..], self.rec[1].stride, 1usize, 1usize),
                (&mut c2[..], self.rec[2].stride, 1usize, 1usize),
            ];
            loop_filter_frame(&mut planes, &self.mi, self.mi_rows, self.mi_cols, h);
            let sse = self.luma_sse_of(&c0);
            if sse < best.1 {
                best = (lvl, sse);
            }
        }
        h.loop_filter_level = best.0;
        self.lf_level = best.0;
        if best.0 > 0 {
            let [p0, p1, p2] = &mut self.rec;
            let mut planes = [
                (&mut p0.buf[..], p0.stride, p0.ss_x, p0.ss_y),
                (&mut p1.buf[..], p1.stride, p1.ss_x, p1.ss_y),
                (&mut p2.buf[..], p2.stride, p2.ss_x, p2.ss_y),
            ];
            loop_filter_frame(&mut planes, &self.mi, self.mi_rows, self.mi_cols, h);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use rff_codec::Decoder;
    use rff_core::{CodecId, Frame, Packet};

    /// C4 — "the house stands": encode a key frame, decode it with our own
    /// decoder, and assert the decoded pixels equal the encoder's reconstruction,
    /// bit-exact (VP9's determinism).
    fn roundtrip(w: u32, h: u32, qindex: u32) {
        let mi_cols = ((w + 7) >> 3) as usize;
        let mi_rows = ((h + 7) >> 3) as usize;
        let (cw, ch) = (mi_cols * 8, mi_rows * 8);
        // A deterministic-ish source: gradients + a little structure.
        let mut s = 0x1234_5678u64;
        let mut rng = || {
            s ^= s << 13;
            s ^= s >> 7;
            s ^= s << 17;
            s
        };
        let y: Vec<u16> = (0..cw * ch)
            .map(|i| {
                let (x, yy) = (i % cw, i / cw);
                ((x + yy + (rng() % 24) as usize) % 256) as u16
            })
            .collect();
        let uv: Vec<u16> = (0..(cw / 2) * (ch / 2))
            .map(|i| (128 + (i % 40) as i32 - 20) as u16)
            .collect();

        let mut enc = FrameEncoder::new(w, h, qindex, [y, uv.clone(), uv], None);
        let bytes = enc.encode_frame();
        // Snapshot the encoder's reconstruction.
        let rec: Vec<Vec<u16>> = enc.recon().iter().map(|p| p.to_vec()).collect();

        // Optional: dump an IVF + our recon for external (libvpx/ffmpeg) validation
        // of a single non-SB-aligned frame (overhang NONE blocks).
        if let Ok(dir) = std::env::var("VP9_RT_OUT") {
            let path = format!("{dir}/f{w}x{h}.ivf");
            let mut ivf = Vec::new();
            ivf.extend_from_slice(b"DKIF");
            ivf.extend_from_slice(&0u16.to_le_bytes());
            ivf.extend_from_slice(&32u16.to_le_bytes());
            ivf.extend_from_slice(b"VP90");
            ivf.extend_from_slice(&(w as u16).to_le_bytes());
            ivf.extend_from_slice(&(h as u16).to_le_bytes());
            ivf.extend_from_slice(&30u32.to_le_bytes());
            ivf.extend_from_slice(&1u32.to_le_bytes());
            ivf.extend_from_slice(&1u32.to_le_bytes());
            ivf.extend_from_slice(&0u32.to_le_bytes());
            ivf.extend_from_slice(&(bytes.len() as u32).to_le_bytes());
            ivf.extend_from_slice(&0u64.to_le_bytes());
            ivf.extend_from_slice(&bytes);
            std::fs::write(&path, &ivf).unwrap();
            let mut raw = Vec::new();
            for p in &rec {
                raw.extend(p.iter().map(|&v| v as u8));
            }
            std::fs::write(format!("{dir}/f{w}x{h}.rec.yuv"), &raw).unwrap();
        }

        // Decode with our own decoder.
        let mut reg = rff_codec::CodecRegistry::new();
        crate::register(&mut reg);
        let mut dec = reg.find_decoder(CodecId::Vp9).unwrap();
        dec.send_packet(&Packet::from_data(0, bytes)).unwrap();
        let frame = dec.receive_frame().expect("a frame");
        let Frame::Video(vf) = frame else {
            panic!("expected video frame")
        };
        assert_eq!((vf.width, vf.height), (w, h));

        // Compare each plane (display size) to the encoder's recon (coded size).
        let dims = [
            (w as usize, h as usize),
            ((w as usize).div_ceil(2), (h as usize).div_ceil(2)),
            ((w as usize).div_ceil(2), (h as usize).div_ceil(2)),
        ];
        for (p, &(pw, ph)) in dims.iter().enumerate() {
            let rec_stride = (mi_cols * 8) >> if p == 0 { 0 } else { 1 };
            let dec_stride = vf.strides[p];
            for yy in 0..ph {
                for xx in 0..pw {
                    let r = rec[p][yy * rec_stride + xx] as u8;
                    let d = vf.planes[p][yy * dec_stride + xx];
                    assert_eq!(r, d, "plane {p} pixel ({xx},{yy})");
                }
            }
        }
    }

    /// GOLDEN reference: frame 2's content matches the key frame (installed as GOLDEN)
    /// but not the previous P (LAST), so the RD should pick GOLDEN for most blocks. The
    /// three-frame stream round-trips bit-exact through our decoder; `VP9_GOLD_OUT`
    /// additionally dumps an IVF + P2 recon for libvpx/ffmpeg validation.
    #[test]
    fn golden_reference_selected_and_roundtrips() {
        let (w, h) = (128u32, 96u32);
        let (cw, ch) = (128usize, 96usize);
        let pat_a = |x: usize, y: usize| (((x * 7) ^ (y * 13)) % 256) as u16;
        let pat_b = |x: usize, y: usize| (((x * 3 + 90) ^ (y * 5 + 40)) % 256) as u16;
        let mkframe = |f: &dyn Fn(usize, usize) -> u16| -> [Vec<u16>; 3] {
            let y: Vec<u16> = (0..cw * ch).map(|i| f(i % cw, i / cw)).collect();
            let uv = vec![128u16; (cw / 2) * (ch / 2)];
            [y, uv.clone(), uv]
        };
        // key = A, P1 = B (unrelated), P2 = A again.
        let mut k = FrameEncoder::new(w, h, 48, mkframe(&pat_a), None);
        let kb = k.encode_frame();
        let krec = k.recon_owned();
        let mut p1 = FrameEncoder::new(w, h, 48, mkframe(&pat_b), Some(krec.clone()));
        p1.set_golden(krec.clone());
        let p1b = p1.encode_frame();
        let p1rec = p1.recon_owned();
        let mut p2 = FrameEncoder::new(w, h, 48, mkframe(&pat_a), Some(p1rec.clone()));
        p2.set_golden(krec.clone());
        let p2b = p2.encode_frame();
        let p2rec = p2.recon_owned();

        // Most of P2 should reference GOLDEN (key ≈ P2), not the unrelated LAST.
        let refs = p2.debug_block_refs();
        let gold = refs
            .iter()
            .filter(|&&r| r == crate::block::GOLDEN_FRAME)
            .count();
        assert!(
            gold > refs.len() / 2,
            "expected majority GOLDEN, got {gold}/{}",
            refs.len()
        );

        // Round-trip: feed key, P1, P2 in order; compare the decoded P2 to its recon.
        let mut reg = rff_codec::CodecRegistry::new();
        crate::register(&mut reg);
        let mut dec = reg.find_decoder(CodecId::Vp9).unwrap();
        let mut last = None;
        for b in [&kb, &p1b, &p2b] {
            dec.send_packet(&Packet::from_data(0, b.clone())).unwrap();
            let Frame::Video(vf) = dec.receive_frame().unwrap() else {
                panic!("video")
            };
            last = Some(vf);
        }
        let vf = last.unwrap();
        for yy in 0..h as usize {
            for xx in 0..w as usize {
                assert_eq!(
                    p2rec[0][yy * cw + xx] as u8,
                    vf.planes[0][yy * vf.strides[0] + xx],
                    "P2 luma ({xx},{yy})"
                );
            }
        }

        if let Ok(dir) = std::env::var("VP9_GOLD_OUT") {
            let mut ivf = Vec::new();
            ivf.extend_from_slice(b"DKIF");
            ivf.extend_from_slice(&0u16.to_le_bytes());
            ivf.extend_from_slice(&32u16.to_le_bytes());
            ivf.extend_from_slice(b"VP90");
            ivf.extend_from_slice(&(w as u16).to_le_bytes());
            ivf.extend_from_slice(&(h as u16).to_le_bytes());
            ivf.extend_from_slice(&30u32.to_le_bytes());
            ivf.extend_from_slice(&1u32.to_le_bytes());
            ivf.extend_from_slice(&3u32.to_le_bytes());
            ivf.extend_from_slice(&0u32.to_le_bytes());
            for (i, b) in [&kb, &p1b, &p2b].iter().enumerate() {
                ivf.extend_from_slice(&(b.len() as u32).to_le_bytes());
                ivf.extend_from_slice(&(i as u64).to_le_bytes());
                ivf.extend_from_slice(b);
            }
            std::fs::write(format!("{dir}/gold.ivf"), &ivf).unwrap();
            let raw: Vec<u8> = p2rec
                .iter()
                .flat_map(|p| p.iter().map(|&v| v as u8))
                .collect();
            std::fs::write(format!("{dir}/gold.p2.yuv"), &raw).unwrap();
        }
    }

    #[test]
    fn keyframe_64x64_roundtrips_bit_exact() {
        roundtrip(64, 64, 40);
    }

    #[test]
    fn keyframe_various_sizes_roundtrip() {
        for &(w, h) in &[(64u32, 64u32), (128, 96), (256, 144)] {
            for &q in &[20u32, 64, 160] {
                roundtrip(w, h, q);
            }
        }
    }

    /// Non-SB-aligned frames where a large NONE block's half-point is in-frame but
    /// the block overhangs the bottom/right edge (its out-of-frame tx blocks are not
    /// coded). `mi_rows=22` (176px) admits a 64×64 overhang NONE at the bottom SB row;
    /// `mi_cols=26` (208px) admits horizontal cases. Bit-exact through our decoder;
    /// `VP9_RT_OUT`/`VP9_RT_RECON` additionally dump for libvpx/ffmpeg validation.
    #[test]
    fn keyframe_overhang_roundtrip() {
        for &(w, h) in &[(256u32, 176u32), (208, 176), (176, 208)] {
            for &q in &[24u32, 96] {
                roundtrip(w, h, q);
            }
        }
    }

    #[test]
    fn pframe_zeromv_roundtrips_bit_exact() {
        for &(w, h) in &[(64u32, 64u32), (128, 96)] {
            let mi_cols = ((w + 7) >> 3) as usize;
            let mi_rows = ((h + 7) >> 3) as usize;
            let (cw, ch) = (mi_cols * 8, mi_rows * 8);
            let gen = |seed: u64| -> [Vec<u16>; 3] {
                let mut s = seed;
                let mut rng = || {
                    s ^= s << 13;
                    s ^= s >> 7;
                    s ^= s << 17;
                    s
                };
                let y: Vec<u16> = (0..cw * ch)
                    .map(|i| ((i % cw + i / cw + (rng() % 24) as usize) % 256) as u16)
                    .collect();
                let uv: Vec<u16> = (0..(cw / 2) * (ch / 2))
                    .map(|i| (128 + (i % 40) as i32 - 20) as u16)
                    .collect();
                [y, uv.clone(), uv]
            };
            // Frame 0: key. Frame 1: P (a *different* source ⇒ a real residual).
            let mut enc0 = FrameEncoder::new(w, h, 48, gen(0xaaaa_aaaa), None);
            let key_bytes = enc0.encode_frame();
            let recon0 = enc0.recon_owned();
            let mut enc1 = FrameEncoder::new(w, h, 48, gen(0xbbbb_bbbb), Some(recon0));
            let p_bytes = enc1.encode_frame();
            let rec1: Vec<Vec<u16>> = enc1.recon().iter().map(|p| p.to_vec()).collect();

            let mut reg = rff_codec::CodecRegistry::new();
            crate::register(&mut reg);
            let mut dec = reg.find_decoder(CodecId::Vp9).unwrap();
            dec.send_packet(&Packet::from_data(0, key_bytes)).unwrap();
            let _ = dec.receive_frame().expect("key frame");
            dec.send_packet(&Packet::from_data(0, p_bytes)).unwrap();
            let Frame::Video(vf) = dec.receive_frame().expect("p frame") else {
                panic!("video")
            };
            assert_eq!((vf.width, vf.height), (w, h));

            let dims = [
                (w as usize, h as usize),
                ((w as usize).div_ceil(2), (h as usize).div_ceil(2)),
                ((w as usize).div_ceil(2), (h as usize).div_ceil(2)),
            ];
            for (p, &(pw, ph)) in dims.iter().enumerate() {
                let rec_stride = (mi_cols * 8) >> if p == 0 { 0 } else { 1 };
                let dec_stride = vf.strides[p];
                for yy in 0..ph {
                    for xx in 0..pw {
                        assert_eq!(
                            rec1[p][yy * rec_stride + xx] as u8,
                            vf.planes[p][yy * dec_stride + xx],
                            "P-frame plane {p} pixel ({xx},{yy}) at {w}x{h}"
                        );
                    }
                }
            }
        }
    }

    /// The motion search must recover a known global shift: encode a key frame,
    /// then a P frame that is the key shifted by a few pixels. Interior blocks
    /// should pick the matching MV, and the result must still round-trip bit-exact.
    #[test]
    fn pframe_newmv_tracks_motion() {
        let (w, h) = (128u32, 96u32);
        let mi_cols = ((w + 7) >> 3) as usize;
        let mi_rows = ((h + 7) >> 3) as usize;
        let (cw, ch) = (mi_cols * 8, mi_rows * 8);
        // A high-frequency texture has a unique local match → an unambiguous SAD
        // minimum at the true shift.
        let px = |x: usize, y: usize| ((x.wrapping_mul(31) ^ y.wrapping_mul(57)) % 256) as u16;
        let y0: Vec<u16> = (0..cw * ch).map(|i| px(i % cw, i / cw)).collect();
        let (dx, dy) = (3usize, 2usize); // shift right 3, down 2
        let y1: Vec<u16> = (0..cw * ch)
            .map(|i| px((i % cw).saturating_sub(dx), (i / cw).saturating_sub(dy)))
            .collect();
        let uv = vec![128u16; (cw / 2) * (ch / 2)];
        let src0 = [y0, uv.clone(), uv.clone()];
        let src1 = [y1, uv.clone(), uv];

        let mut enc0 = FrameEncoder::new(w, h, 32, src0, None);
        let key = enc0.encode_frame();
        let recon0 = enc0.recon_owned();
        let mut enc1 = FrameEncoder::new(w, h, 32, src1, Some(recon0));
        let p = enc1.encode_frame();
        let rec1: Vec<Vec<u16>> = enc1.recon().iter().map(|q| q.to_vec()).collect();

        // The MC fetches the reference at `base + mv`, so recovering a +shift in
        // the source needs a −shift MV.
        let want = (-(dy as i32) * 8, -(dx as i32) * 8);
        let mvs = enc1.debug_block_mvs();
        let (mut hit, mut total) = (0usize, 0usize);
        for r in 2..mi_rows - 1 {
            for c in 2..mi_cols - 1 {
                total += 1;
                if mvs[r * mi_cols + c] == want {
                    hit += 1;
                }
            }
        }
        assert!(
            hit * 2 > total,
            "motion search recovered the shift in only {hit}/{total} interior blocks"
        );

        // Bit-exact through the decoder.
        let mut reg = rff_codec::CodecRegistry::new();
        crate::register(&mut reg);
        let mut dec = reg.find_decoder(CodecId::Vp9).unwrap();
        dec.send_packet(&Packet::from_data(0, key)).unwrap();
        let _ = dec.receive_frame().unwrap();
        dec.send_packet(&Packet::from_data(0, p)).unwrap();
        let Frame::Video(vf) = dec.receive_frame().unwrap() else {
            panic!("video")
        };
        let dec_stride = vf.strides[0];
        for yy in 0..h as usize {
            for xx in 0..w as usize {
                assert_eq!(
                    rec1[0][yy * cw + xx] as u8,
                    vf.planes[0][yy * dec_stride + xx],
                    "P-frame luma ({xx},{yy})"
                );
            }
        }
    }

    /// The subpel refinement must recover a half-pel motion. We synthesise the P
    /// frame's luma as an *exact* half-pel (mv = (0,−4)) motion-compensation of the
    /// key-frame reconstruction, so the optimal MV is provably fractional and gives
    /// zero SAD. Interior blocks must pick (0,−4), and it must round-trip bit-exact.
    #[test]
    fn pframe_newmv_subpel() {
        let (w, h) = (96u32, 64u32);
        let mi_cols = ((w + 7) >> 3) as usize;
        let mi_rows = ((h + 7) >> 3) as usize;
        let (cw, ch) = (mi_cols * 8, mi_rows * 8);
        let y0: Vec<u16> = (0..cw * ch)
            .map(|i| ((i % cw).wrapping_mul(31) ^ (i / cw).wrapping_mul(57)) as u16 % 256)
            .collect();
        let flat = vec![128u16; (cw / 2) * (ch / 2)];
        let mut enc0 = FrameEncoder::new(w, h, 24, [y0, flat.clone(), flat.clone()], None);
        let key = enc0.encode_frame();
        let recon0 = enc0.recon_owned();

        // P luma = per-block half-pel-left MC of the recon (mv = (0,−4): bx−1,
        // horizontal subpel phase 8) — exactly what `inter_predict_mv` produces.
        let rp = RefPlane {
            buf: &recon0[0],
            stride: cw,
            w: cw as i32,
            h: ch as i32,
        };
        let mut y1 = vec![0u16; cw * ch];
        for by in (0..ch).step_by(8) {
            for bx in (0..cw).step_by(8) {
                let mut pred = [0u16; 64];
                predict_block(
                    &rp,
                    bx as i32 - 1,
                    by as i32,
                    8,
                    0,
                    0,
                    &mut pred,
                    8,
                    8,
                    8,
                    false,
                    255,
                );
                for yy in 0..8 {
                    for xx in 0..8 {
                        y1[(by + yy) * cw + bx + xx] = pred[yy * 8 + xx];
                    }
                }
            }
        }
        let mut enc1 = FrameEncoder::new(
            w,
            h,
            24,
            [y1, recon0[1].clone(), recon0[2].clone()],
            Some(recon0.clone()),
        );
        let p = enc1.encode_frame();
        let rec1: Vec<Vec<u16>> = enc1.recon().iter().map(|q| q.to_vec()).collect();

        // Interior blocks should pick the half-pel MV (0, −4).
        let mvs = enc1.debug_block_mvs();
        let (mut hit, mut total) = (0usize, 0usize);
        for r in 1..mi_rows - 1 {
            for c in 2..mi_cols - 1 {
                total += 1;
                if mvs[r * mi_cols + c] == (0, -4) {
                    hit += 1;
                }
            }
        }
        assert!(
            hit * 2 > total,
            "subpel search found the half-pel MV in only {hit}/{total} interior blocks"
        );

        let mut reg = rff_codec::CodecRegistry::new();
        crate::register(&mut reg);
        let mut dec = reg.find_decoder(CodecId::Vp9).unwrap();
        dec.send_packet(&Packet::from_data(0, key)).unwrap();
        let _ = dec.receive_frame().unwrap();
        dec.send_packet(&Packet::from_data(0, p)).unwrap();
        let Frame::Video(vf) = dec.receive_frame().unwrap() else {
            panic!("video")
        };
        for yy in 0..h as usize {
            for xx in 0..w as usize {
                assert_eq!(
                    rec1[0][yy * cw + xx] as u8,
                    vf.planes[0][yy * vf.strides[0] + xx],
                    "subpel P-frame luma ({xx},{yy})"
                );
            }
        }
    }

    /// The intra-vs-inter decision must fall back to intra for content the
    /// reference cannot predict. The key frame is random texture; the P frame is a
    /// smooth horizontal ramp (constant down each column) that V_PRED predicts
    /// almost perfectly while no MV into the texture can. Interior blocks should go
    /// intra (V_PRED), and it must round-trip bit-exact.
    #[test]
    fn pframe_intra_fallback() {
        let (w, h) = (96u32, 64u32);
        let mi_cols = ((w + 7) >> 3) as usize;
        let mi_rows = ((h + 7) >> 3) as usize;
        let (cw, ch) = (mi_cols * 8, mi_rows * 8);
        let tex: Vec<u16> = (0..cw * ch)
            .map(|i| ((i % cw).wrapping_mul(31) ^ (i / cw).wrapping_mul(57)) as u16 % 256)
            .collect();
        // Horizontal ramp, identical every row ⇒ V_PRED (copy the row above) is exact.
        let ramp: Vec<u16> = (0..cw * ch).map(|i| ((i % cw) * 255 / cw) as u16).collect();
        let flat = vec![128u16; (cw / 2) * (ch / 2)];

        let mut enc0 = FrameEncoder::new(w, h, 24, [tex, flat.clone(), flat.clone()], None);
        let key = enc0.encode_frame();
        let recon0 = enc0.recon_owned();
        let mut enc1 = FrameEncoder::new(w, h, 24, [ramp, flat.clone(), flat], Some(recon0));
        let p = enc1.encode_frame();
        let rec1: Vec<Vec<u16>> = enc1.recon().iter().map(|q| q.to_vec()).collect();

        let modes = enc1.debug_block_modes();
        let (mut intra_v, mut total) = (0usize, 0usize);
        for r in 1..mi_rows - 1 {
            for c in 1..mi_cols - 1 {
                total += 1;
                let (is_inter, mode) = modes[r * mi_cols + c];
                if !is_inter && mode == V_PRED {
                    intra_v += 1;
                }
            }
        }
        assert!(
            intra_v * 2 > total,
            "intra fallback chosen for only {intra_v}/{total} interior blocks"
        );

        let mut reg = rff_codec::CodecRegistry::new();
        crate::register(&mut reg);
        let mut dec = reg.find_decoder(CodecId::Vp9).unwrap();
        dec.send_packet(&Packet::from_data(0, key)).unwrap();
        let _ = dec.receive_frame().unwrap();
        dec.send_packet(&Packet::from_data(0, p)).unwrap();
        let Frame::Video(vf) = dec.receive_frame().unwrap() else {
            panic!("video")
        };
        for yy in 0..h as usize {
            for xx in 0..w as usize {
                assert_eq!(
                    rec1[0][yy * cw + xx] as u8,
                    vf.planes[0][yy * vf.strides[0] + xx],
                    "intra-fallback P-frame luma ({xx},{yy})"
                );
            }
        }
    }

    /// R1 — RDO yields a better rate/distortion point than distortion-only mode
    /// selection: at the same `qindex`, the rate term buys a smaller file for
    /// near-identical quality.
    #[test]
    fn rdo_improves_rate_distortion() {
        let (w, h) = (128u32, 128u32);
        let mi_cols = ((w + 7) >> 3) as usize;
        let mi_rows = ((h + 7) >> 3) as usize;
        let (cw, ch) = (mi_cols * 8, mi_rows * 8);
        // Mixed content (gradients + high-frequency) so intra modes genuinely
        // trade distortion against rate.
        let y: Vec<u16> = (0..cw * ch)
            .map(|i| {
                let (x, yy) = (i % cw, i / cw);
                (((x * 3) ^ (yy * 2)).wrapping_add(x * yy / 16) % 256) as u16
            })
            .collect();
        let uv: Vec<u16> = (0..(cw / 2) * (ch / 2))
            .map(|i| (128 + (i % 50) as i32 - 25) as u16)
            .collect();
        let src = [y, uv.clone(), uv];

        let run = |rdo: bool| -> (usize, u64) {
            let mut enc = FrameEncoder::new(w, h, 80, src.clone(), None);
            enc.set_use_rdo(rdo);
            let bytes = enc.encode_frame();
            let rec = enc.recon();
            let mut sse = 0u64;
            for i in 0..cw * ch {
                let d = src[0][i] as i64 - rec[0][i] as i64;
                sse += (d * d) as u64;
            }
            (bytes.len(), sse)
        };
        let (bits_dist, sse_dist) = run(false);
        let (bits_rdo, sse_rdo) = run(true);

        // RDO produces a strictly smaller file...
        assert!(
            bits_rdo < bits_dist,
            "RDO did not reduce size: {bits_rdo} vs {bits_dist} bytes"
        );
        // ...at near-equal luma distortion (within ~0.7 dB PSNR ⇒ ≤ ~17% SSE).
        assert!(
            sse_rdo as f64 <= sse_dist as f64 * 1.17,
            "RDO distortion grew too much: sse {sse_rdo} vs {sse_dist}"
        );
        let savings = 100.0 * (bits_dist - bits_rdo) as f64 / bits_dist as f64;
        eprintln!(
            "RDO: {bits_rdo} vs {bits_dist} bytes ({savings:.1}% smaller), sse {sse_rdo} vs {sse_dist}"
        );
    }

    /// R3 — the loop-filter search engages on smooth content coarsely quantized
    /// (blocking artifacts the deblocker removes), picking a level > 0, and the
    /// deblocked frame still round-trips bit-exact through the decoder.
    #[test]
    fn loop_filter_engages_and_roundtrips() {
        let (w, h) = (128u32, 96u32);
        let mi_cols = ((w + 7) >> 3) as usize;
        let mi_rows = ((h + 7) >> 3) as usize;
        let (cw, ch) = (mi_cols * 8, mi_rows * 8);
        // A smooth gradient ⇒ coarse quantization leaves visible block edges that
        // the deblocking filter (toward the smooth source) removes.
        let y: Vec<u16> = (0..cw * ch)
            .map(|i| (40 + (i % cw) * 150 / cw + (i / cw) * 60 / ch) as u16)
            .collect();
        let uv = vec![128u16; (cw / 2) * (ch / 2)];
        let src = [y, uv.clone(), uv];

        let mut enc = FrameEncoder::new(w, h, 180, src, None); // high q ⇒ blocking
        let bytes = enc.encode_frame();
        assert!(enc.lf_level() > 0, "loop filter not engaged (level 0)");
        let rec0: Vec<Vec<u16>> = enc.recon().iter().map(|p| p.to_vec()).collect();

        let mut reg = rff_codec::CodecRegistry::new();
        crate::register(&mut reg);
        let mut dec = reg.find_decoder(CodecId::Vp9).unwrap();
        dec.send_packet(&Packet::from_data(0, bytes)).unwrap();
        let Frame::Video(vf) = dec.receive_frame().unwrap() else {
            panic!("video")
        };
        // The decoder reproduces our deblocked reconstruction exactly.
        for yy in 0..h as usize {
            for xx in 0..w as usize {
                assert_eq!(
                    rec0[0][yy * cw + xx] as u8,
                    vf.planes[0][yy * vf.strides[0] + xx],
                    "deblocked luma ({xx},{yy}) at level {}",
                    enc.lf_level()
                );
            }
        }
    }

    /// R4 — forward coefficient-prob updates shrink the frame across a *corpus* of
    /// varied content (not one clip — the tune-quality discipline), and every
    /// updated frame still round-trips bit-exact through the decoder.
    #[test]
    fn prob_updates_shrink_corpus_bit_exact() {
        let (w, h) = (128u32, 128u32);
        let mi_cols = ((w + 7) >> 3) as usize;
        let mi_rows = ((h + 7) >> 3) as usize;
        let (cw, ch) = (mi_cols * 8, mi_rows * 8);
        // Five distinct luma fields: gradient, high-freq texture, blocky regions,
        // diagonal ramp, mixed — varied coefficient statistics.
        let fields: [fn(usize, usize) -> u16; 5] = [
            |x, y| (20 + x + y) as u16 % 256,
            |x, y| (x.wrapping_mul(53) ^ y.wrapping_mul(97)) as u16 % 256,
            |x, y| (((x / 16) + (y / 16)) * 37) as u16 % 256,
            |x, y| (x * 2 + y / 2) as u16 % 256,
            |x, y| ((x * y) / 8 + (x ^ y)) as u16 % 256,
        ];

        let (mut total_off, mut total_on) = (0usize, 0usize);
        for field in fields {
            let y: Vec<u16> = (0..cw * ch).map(|i| field(i % cw, i / cw)).collect();
            let uv = vec![128u16; (cw / 2) * (ch / 2)];
            let src = [y, uv.clone(), uv];

            let mut off = FrameEncoder::new(w, h, 64, src.clone(), None);
            off.set_use_prob_updates(false);
            total_off += off.encode_frame().len();

            let mut on = FrameEncoder::new(w, h, 64, src, None);
            let bytes = on.encode_frame();
            total_on += bytes.len();
            let rec: Vec<Vec<u16>> = on.recon().iter().map(|p| p.to_vec()).collect();

            // Bit-exact: the decoder reproduces the prob-updated frame exactly.
            let mut reg = rff_codec::CodecRegistry::new();
            crate::register(&mut reg);
            let mut dec = reg.find_decoder(CodecId::Vp9).unwrap();
            dec.send_packet(&Packet::from_data(0, bytes)).unwrap();
            let Frame::Video(vf) = dec.receive_frame().unwrap() else {
                panic!("video")
            };
            for yy in 0..h as usize {
                for xx in 0..w as usize {
                    assert_eq!(
                        rec[0][yy * cw + xx] as u8,
                        vf.planes[0][yy * vf.strides[0] + xx],
                        "prob-update luma ({xx},{yy})"
                    );
                }
            }
        }
        let savings = 100.0 * (total_off - total_on) as f64 / total_off as f64;
        eprintln!("R4: corpus {total_on} vs {total_off} bytes ({savings:.1}% smaller)");
        assert!(
            total_on < total_off,
            "prob updates grew the corpus: {total_on} vs {total_off}"
        );
    }

    /// R5 — the worked example of the biased-`J` trap. At the *original* (too-high)
    /// λ the AC deadzone lowers the encoder's own RD cost `J = SSE + λ·bits` and
    /// stays bit-exact — looks like a win. It is not: the BD-rate oracle scored it
    /// +1.66% (a loss), and λ-calibration (now λ=ac²·0.001) so lowers λ that the
    /// deadzone no longer even fools `J`. So this pins the old λ to preserve the
    /// demonstration; the deadzone ships OFF (round-to-nearest).
    #[test]
    fn deadzone_lowers_self_metric_j_bit_exact() {
        let (w, h) = (96u32, 96u32);
        let mi_cols = ((w + 7) >> 3) as usize;
        let mi_rows = ((h + 7) >> 3) as usize;
        let (cw, ch) = (mi_cols * 8, mi_rows * 8);
        let fields: [fn(usize, usize) -> u16; 5] = [
            |x, y| (20 + x + y) as u16 % 256,
            |x, y| (x.wrapping_mul(53) ^ y.wrapping_mul(97)) as u16 % 256,
            |x, y| (((x / 16) + (y / 16)) * 37) as u16 % 256,
            |x, y| (x * 2 + y / 2) as u16 % 256,
            |x, y| ((x * y) / 8 + (x ^ y)) as u16 % 256,
        ];

        let (mut j_off, mut j_on) = (0.0f64, 0.0f64);
        let (mut bits_off, mut bits_on, mut sse_off, mut sse_on) = (0usize, 0usize, 0u64, 0u64);
        for field in fields {
            let y: Vec<u16> = (0..cw * ch).map(|i| field(i % cw, i / cw)).collect();
            let uv = vec![128u16; (cw / 2) * (ch / 2)];
            let src = [y, uv.clone(), uv];

            let run = |round_num: i64| -> (usize, u64, f64, Vec<u16>) {
                let mut enc = FrameEncoder::new(w, h, 64, src.clone(), None);
                enc.set_use_prob_updates(false); // isolate the deadzone knob
                enc.set_use_trellis(false);
                enc.set_lambda_mult(0.02); // the original biased λ where J is fooled
                enc.set_ac_round_num(round_num);
                let bytes = enc.encode_frame();
                let rec = enc.recon();
                let mut sse = 0u64;
                for i in 0..cw * ch {
                    let d = src[0][i] as i64 - rec[0][i] as i64;
                    sse += (d * d) as u64;
                }
                (bytes.len(), sse, enc.lambda(), rec[0].to_vec())
            };
            let (b_off, s_off, lam, _) = run(4); // round-to-nearest
            let (b_on, s_on, _, rec_on) = run(3); // deadzone
            bits_off += b_off;
            bits_on += b_on;
            sse_off += s_off;
            sse_on += s_on;
            j_off += s_off as f64 + lam * (b_off as f64 * 8.0);
            j_on += s_on as f64 + lam * (b_on as f64 * 8.0);

            // The deadzoned frame must still decode bit-exact (same config as run(3)).
            let mut enc = FrameEncoder::new(w, h, 64, src.clone(), None);
            enc.set_use_prob_updates(false);
            enc.set_use_trellis(false);
            enc.set_lambda_mult(0.02);
            enc.set_ac_round_num(3);
            let bytes = enc.encode_frame();
            let mut reg = rff_codec::CodecRegistry::new();
            crate::register(&mut reg);
            let mut dec = reg.find_decoder(CodecId::Vp9).unwrap();
            dec.send_packet(&Packet::from_data(0, bytes)).unwrap();
            let Frame::Video(vf) = dec.receive_frame().unwrap() else {
                panic!("video")
            };
            for yy in 0..h as usize {
                for xx in 0..w as usize {
                    assert_eq!(
                        rec_on[yy * cw + xx] as u8,
                        vf.planes[0][yy * vf.strides[0] + xx],
                        "deadzone luma ({xx},{yy})"
                    );
                }
            }
        }
        eprintln!(
            "deadzone: J {j_on:.0} vs {j_off:.0}; bits {bits_on} vs {bits_off}; sse {sse_on} vs {sse_off}"
        );
        assert!(
            j_on < j_off,
            "deadzone did not improve RD: J {j_on:.0} (on) vs {j_off:.0} (off)"
        );
    }

    /// Roof — tx-size search engages (picks 8×8 on smooth content) and the frame
    /// still round-trips bit-exact through the decoder (the 8×8 forward+inverse path
    /// must match, and the per-block tx_size bits must parse).
    #[test]
    fn tx_search_engages_and_roundtrips() {
        let (w, h) = (128u32, 96u32);
        let mi_cols = ((w + 7) >> 3) as usize;
        let mi_rows = ((h + 7) >> 3) as usize;
        let (cw, ch) = (mi_cols * 8, mi_rows * 8);
        // Smooth gradient ⇒ an 8×8 transform codes the residual in fewer coefs.
        let y: Vec<u16> = (0..cw * ch)
            .map(|i| (30 + (i % cw) * 120 / cw + (i / cw) * 60 / ch) as u16)
            .collect();
        let uv = vec![128u16; (cw / 2) * (ch / 2)];
        let mut enc = FrameEncoder::new(w, h, 96, [y, uv.clone(), uv], None);
        enc.set_use_partition_rd(false); // isolate tx-search in the fixed-8×8 regime
        enc.set_use_tx_search(true);
        let bytes = enc.encode_frame();
        let rec: Vec<Vec<u16>> = enc.recon().iter().map(|p| p.to_vec()).collect();

        let n8 = enc
            .debug_block_tx_sizes()
            .iter()
            .filter(|&&t| t == 1)
            .count();
        let total = mi_rows * mi_cols;
        assert!(n8 > total / 4, "tx-search rarely picked 8×8: {n8}/{total}");

        let mut reg = rff_codec::CodecRegistry::new();
        crate::register(&mut reg);
        let mut dec = reg.find_decoder(CodecId::Vp9).unwrap();
        dec.send_packet(&Packet::from_data(0, bytes)).unwrap();
        let Frame::Video(vf) = dec.receive_frame().unwrap() else {
            panic!("video")
        };
        for yy in 0..h as usize {
            for xx in 0..w as usize {
                assert_eq!(
                    rec[0][yy * cw + xx] as u8,
                    vf.planes[0][yy * vf.strides[0] + xx],
                    "tx-search luma ({xx},{yy})"
                );
            }
        }
    }

    /// Roof bring-up — larger blocks (16×16 / 32×32 coded as PARTITION_NONE) must
    /// encode and round-trip bit-exact through the decoder before the partition RD
    /// can pick them. Exercises the never-before-used ≥16×16 intra + tx geometry.
    #[test]
    fn larger_blocks_roundtrip_bit_exact() {
        let (w, h) = (64u32, 64u32); // divides evenly into 16×16 and 32×32
        let (cw, ch) = (64usize, 64usize);
        let y: Vec<u16> = (0..cw * ch)
            .map(|i| ((i % cw).wrapping_mul(29) ^ (i / cw).wrapping_mul(43)) as u16 % 256)
            .collect();
        let uv = vec![128u16; (cw / 2) * (ch / 2)];
        let src = [y, uv.clone(), uv];

        // VP9 block-size enum: BLOCK_16X16 = 6, BLOCK_32X32 = 9.
        for &bs in &[6usize, 9] {
            let mut enc = FrameEncoder::new(w, h, 64, src.clone(), None);
            enc.set_use_partition_rd(false); // exercise the fixed force_min_bsize path
            enc.set_force_min_bsize(bs); // full default path: tx-search + trellis + R4
            let bytes = enc.encode_frame();
            let rec: Vec<Vec<u16>> = enc.recon().iter().map(|p| p.to_vec()).collect();
            assert!(
                enc.debug_block_sizes().iter().any(|&s| s as usize == bs),
                "no block of size {bs} was coded"
            );

            let mut reg = rff_codec::CodecRegistry::new();
            crate::register(&mut reg);
            let mut dec = reg.find_decoder(CodecId::Vp9).unwrap();
            dec.send_packet(&Packet::from_data(0, bytes)).unwrap();
            let Frame::Video(vf) = dec.receive_frame().unwrap() else {
                panic!("video")
            };
            for yy in 0..h as usize {
                for xx in 0..w as usize {
                    assert_eq!(
                        rec[0][yy * cw + xx] as u8,
                        vf.planes[0][yy * vf.strides[0] + xx],
                        "bsize {bs} luma ({xx},{yy})"
                    );
                }
            }
        }
    }

    /// Recursive partition RD: on a frame with a flat half (wants big NONE blocks)
    /// and a detailed half (wants SPLIT down to 8×8), the RD search must pick a
    /// *mix* of partition sizes and the result must still round-trip bit-exact.
    #[test]
    fn partition_rd_roundtrip_and_adapts() {
        let (w, h) = (128u32, 128u32);
        let (cw, ch) = (128usize, 128usize);
        // Left half: flat (128) → a 64×64 NONE predicts it perfectly. Right half: a
        // steep *diagonal* gradient. The encoder's intra modes (DC/V/H/TM) capture
        // flats and 1-D ramps at any size, but not a diagonal — a 64×64 NONE leaves
        // a large residual, while small blocks track the gradient in steps. So the
        // RD must keep NONE left and SPLIT right.
        let y: Vec<u16> = (0..cw * ch)
            .map(|i| {
                let (x, yy) = (i % cw, i / cw);
                if x < cw / 2 {
                    128
                } else {
                    (20 + ((x + yy) * 3) % 200) as u16
                }
            })
            .collect();
        let uv = vec![128u16; (cw / 2) * (ch / 2)];
        let src = [y, uv.clone(), uv];

        let mut enc = FrameEncoder::new(w, h, 96, src.clone(), None);
        enc.set_use_partition_rd(true);
        let bytes = enc.encode_frame();
        let rec: Vec<Vec<u16>> = enc.recon().iter().map(|p| p.to_vec()).collect();

        // The decision must adapt: more than one distinct block size in the frame.
        let sizes: std::collections::HashSet<u8> =
            enc.debug_block_sizes().iter().copied().collect();
        assert!(
            sizes.len() >= 2,
            "partition RD produced a single block size {sizes:?} — not adapting"
        );

        let mut reg = rff_codec::CodecRegistry::new();
        crate::register(&mut reg);
        let mut dec = reg.find_decoder(CodecId::Vp9).unwrap();
        dec.send_packet(&Packet::from_data(0, bytes)).unwrap();
        let Frame::Video(vf) = dec.receive_frame().unwrap() else {
            panic!("video")
        };
        for yy in 0..h as usize {
            for xx in 0..w as usize {
                assert_eq!(
                    rec[0][yy * cw + xx] as u8,
                    vf.planes[0][yy * vf.strides[0] + xx],
                    "partition-rd luma ({xx},{yy})"
                );
            }
        }
    }

    /// Emit a one-frame IVF to `VP9_ENC_OUT` so an external decoder (libvpx /
    /// ffmpeg) can validate our bitstream is *legal*, not just self-tolerated.
    #[test]
    #[ignore = "writes an IVF to VP9_ENC_OUT for external ffmpeg/libvpx validation"]
    fn emit_ivf_for_external_decode() {
        let path = std::env::var("VP9_ENC_OUT").expect("set VP9_ENC_OUT");
        let (w, h) = (256u32, 240u32);
        let mi_cols = ((w + 7) >> 3) as usize;
        let mi_rows = ((h + 7) >> 3) as usize;
        let (cw, ch) = (mi_cols * 8, mi_rows * 8);
        // A textured pattern, shifted between frames so the P frame carries real
        // NEWMV motion vectors (not just ZEROMV) for ffmpeg to validate.
        let pat = |x: usize, y: usize| ((x.wrapping_mul(31) ^ y.wrapping_mul(57)) % 256) as u16;
        let frame = |sx: usize, sy: usize| -> [Vec<u16>; 3] {
            let y: Vec<u16> = (0..cw * ch)
                .map(|i| pat((i % cw).saturating_sub(sx), (i / cw).saturating_sub(sy)))
                .collect();
            let uv: Vec<u16> = (0..(cw / 2) * (ch / 2))
                .map(|i| (128 + (i % 64) as i32 - 32) as u16)
                .collect();
            [y, uv.clone(), uv]
        };
        // A key frame, then a P frame shifted right 4 / down 2 against it.
        let mut enc0 = FrameEncoder::new(w, h, 48, frame(0, 0), None);
        let key = enc0.encode_frame();
        let recon0 = enc0.recon_owned();
        let mut enc1 = FrameEncoder::new(w, h, 48, frame(4, 2), Some(recon0.clone()));
        let pframe = enc1.encode_frame();
        let frames = [key, pframe];

        let mut ivf = Vec::new();
        ivf.extend_from_slice(b"DKIF");
        ivf.extend_from_slice(&0u16.to_le_bytes()); // version
        ivf.extend_from_slice(&32u16.to_le_bytes()); // header length
        ivf.extend_from_slice(b"VP90");
        ivf.extend_from_slice(&(w as u16).to_le_bytes());
        ivf.extend_from_slice(&(h as u16).to_le_bytes());
        ivf.extend_from_slice(&30u32.to_le_bytes()); // fps num
        ivf.extend_from_slice(&1u32.to_le_bytes()); // fps den
        ivf.extend_from_slice(&(frames.len() as u32).to_le_bytes()); // frame count
        ivf.extend_from_slice(&0u32.to_le_bytes()); // unused
        for (i, frame) in frames.iter().enumerate() {
            ivf.extend_from_slice(&(frame.len() as u32).to_le_bytes());
            ivf.extend_from_slice(&(i as u64).to_le_bytes()); // timestamp
            ivf.extend_from_slice(frame);
        }
        std::fs::write(&path, &ivf).unwrap();
        eprintln!(
            "wrote {} bytes IVF ({}x{}, key+P) to {path}",
            ivf.len(),
            w,
            h
        );

        // Optionally dump our own reconstruction as raw YUV420p (here coded size
        // == display size) so it can be diffed against the external decoder.
        if let Ok(recon_path) = std::env::var("VP9_ENC_RECON") {
            let mut raw = Vec::new();
            for rec in [&recon0, &enc1.recon().map(|p| p.to_vec())] {
                for plane in rec.iter() {
                    raw.extend(plane.iter().map(|&v| v as u8));
                }
            }
            std::fs::write(&recon_path, &raw).unwrap();
            eprintln!("wrote {} bytes recon YUV to {recon_path}", raw.len());
        }
    }

    /// A multi-frame IPPP… GOP round-trips bit-exact: every P frame references the
    /// previous reconstruction, so a reference drift would compound frame-over-frame.
    /// Set `VP9_GOP_OUT` (+ optionally `VP9_GOP_RECON`) to also dump an IVF + our
    /// reconstruction for external (libvpx/ffmpeg) pixel validation.
    #[test]
    fn multiframe_gop_roundtrips_bit_exact() {
        let (w, h) = (128u32, 96u32);
        let (cw, ch) = (128usize, 96usize);
        // A texture that pans by (2,1) px/frame (real NEWMV motion) — the newly
        // revealed edges carry residual, the interior re-predicts (mix of skip/inter).
        let tex = |x: usize, y: usize| ((x.wrapping_mul(31) ^ y.wrapping_mul(57)) % 256) as u16;
        let frame = |t: usize| -> [Vec<u16>; 3] {
            let y: Vec<u16> = (0..cw * ch)
                .map(|i| tex((i % cw).saturating_sub(t * 2), (i / cw).saturating_sub(t)))
                .collect();
            let uv = vec![128u16; (cw / 2) * (ch / 2)];
            [y, uv.clone(), uv]
        };
        let n = 12usize;
        let mut refr: Option<[Vec<u16>; 3]> = None;
        let (mut streams, mut recons) = (Vec::new(), Vec::new());
        for t in 0..n {
            let mut enc = FrameEncoder::new(w, h, 64, frame(t), refr.take());
            let bytes = enc.encode_frame();
            let recon = enc.recon_owned();
            refr = Some(recon.clone());
            streams.push(bytes);
            recons.push(recon);
        }
        // Round-trip every frame through our decoder against the encoder recon.
        let mut reg = rff_codec::CodecRegistry::new();
        crate::register(&mut reg);
        let mut dec = reg.find_decoder(CodecId::Vp9).unwrap();
        for t in 0..n {
            dec.send_packet(&Packet::from_data(0, streams[t].clone()))
                .unwrap();
            let Frame::Video(vf) = dec.receive_frame().unwrap() else {
                panic!("video")
            };
            for yy in 0..h as usize {
                for xx in 0..w as usize {
                    assert_eq!(
                        recons[t][0][yy * cw + xx] as u8,
                        vf.planes[0][yy * vf.strides[0] + xx],
                        "GOP frame {t} luma ({xx},{yy})"
                    );
                }
            }
        }
        if let Ok(path) = std::env::var("VP9_GOP_OUT") {
            let mut ivf = Vec::new();
            ivf.extend_from_slice(b"DKIF");
            ivf.extend_from_slice(&0u16.to_le_bytes());
            ivf.extend_from_slice(&32u16.to_le_bytes());
            ivf.extend_from_slice(b"VP90");
            ivf.extend_from_slice(&(w as u16).to_le_bytes());
            ivf.extend_from_slice(&(h as u16).to_le_bytes());
            ivf.extend_from_slice(&30u32.to_le_bytes());
            ivf.extend_from_slice(&1u32.to_le_bytes());
            ivf.extend_from_slice(&(n as u32).to_le_bytes());
            ivf.extend_from_slice(&0u32.to_le_bytes());
            for (i, f) in streams.iter().enumerate() {
                ivf.extend_from_slice(&(f.len() as u32).to_le_bytes());
                ivf.extend_from_slice(&(i as u64).to_le_bytes());
                ivf.extend_from_slice(f);
            }
            std::fs::write(&path, &ivf).unwrap();
            if let Ok(rp) = std::env::var("VP9_GOP_RECON") {
                let mut raw = Vec::new();
                for rec in &recons {
                    for plane in rec.iter() {
                        raw.extend(plane.iter().map(|&v| v as u8));
                    }
                }
                std::fs::write(&rp, &raw).unwrap();
            }
        }
    }

    /// A P frame that perfectly re-predicts most of its blocks (ZEROMV, no residual)
    /// must code them `skip` — coding skip=false with empty EOB tokens is decoded
    /// consistently by *us* but drifts a conformant decoder (libvpx/ffmpeg). Assert
    /// the skip path engages and the frame still round-trips bit-exact.
    #[test]
    fn inter_empty_blocks_coded_as_skip() {
        let (w, h) = (64u32, 64u32);
        let (cw, ch) = (64usize, 64usize);
        let pat = |x: usize, y: usize| ((x.wrapping_mul(31) ^ y.wrapping_mul(57)) % 256) as u16;
        let src = || -> [Vec<u16>; 3] {
            let y: Vec<u16> = (0..cw * ch).map(|i| pat(i % cw, i / cw)).collect();
            let uv = vec![128u16; (cw / 2) * (ch / 2)];
            [y, uv.clone(), uv]
        };
        let mut enc0 = FrameEncoder::new(w, h, 48, src(), None);
        let key = enc0.encode_frame();
        let recon0 = enc0.recon_owned();
        // Same content ⇒ ZEROMV re-predicts it exactly ⇒ most blocks empty ⇒ skip.
        let mut enc1 = FrameEncoder::new(w, h, 48, src(), Some(recon0));
        let bytes = enc1.encode_frame();
        assert!(
            enc1.debug_skip_count() > 0,
            "no inter block was coded as skip — the empty-block fix didn't engage"
        );
        let rec: Vec<Vec<u16>> = enc1.recon().iter().map(|p| p.to_vec()).collect();

        let mut reg = rff_codec::CodecRegistry::new();
        crate::register(&mut reg);
        let mut dec = reg.find_decoder(CodecId::Vp9).unwrap();
        dec.send_packet(&Packet::from_data(0, key)).unwrap(); // key first (the P ref)
        let _ = dec.receive_frame().unwrap();
        dec.send_packet(&Packet::from_data(0, bytes)).unwrap();
        let Frame::Video(vf) = dec.receive_frame().unwrap() else {
            panic!("video")
        };
        for yy in 0..h as usize {
            for xx in 0..w as usize {
                assert_eq!(
                    rec[0][yy * cw + xx] as u8,
                    vf.planes[0][yy * vf.strides[0] + xx],
                    "skip P-frame luma ({xx},{yy})"
                );
            }
        }
    }
}
