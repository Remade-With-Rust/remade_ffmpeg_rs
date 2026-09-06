//! Per-picture decoding state at 4×4 (minimum transform block) granularity:
//! the z-scan order table (§6.5.2), neighbour availability (§6.4.1), and the
//! per-block maps the syntax, intra prediction and loop filters read.

use crate::ps::{Sps, TileLayout};

/// `CuPredMode` per 4×4.
pub const PRED_NONE: u8 = 0;
pub const PRED_INTRA: u8 = 1;
pub const PRED_INTER: u8 = 2;
pub const PRED_SKIP: u8 = 3;

/// SAO parameters of one CTB for one component.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct SaoParams {
    /// 0 = off, 1 = band, 2 = edge.
    pub type_idx: u8,
    /// Band position or edge-offset class.
    pub aux: u8,
    /// Offsets (band: 4 consecutive bands from `aux`; edge: categories 1..=4).
    pub offset: [i16; 4],
}

/// Motion data of one 4×4 (Phase 3 fills it; deblocking reads it).
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct Motion {
    pub mv: [[i16; 2]; 2],
    pub ref_idx: [i8; 2],
    /// Bit 0 = L0 used, bit 1 = L1 used.
    pub pred_flags: u8,
    /// POC of the referenced pictures (for boundary strength: "same picture" test).
    pub ref_poc: [i32; 2],
    /// Bit l set = the list-l reference was a long-term picture when this
    /// picture was decoded (`LongTermRefPic` for TMVP).
    pub ref_lt: u8,
}

#[derive(Debug, Clone, Copy, Default)]
pub struct CtbFilterParams {
    pub deblock_disabled: bool,
    pub beta_offset_div2: i8,
    pub tc_offset_div2: i8,
    pub lf_across_slices: bool,
    pub cb_qp_offset: i8,
    pub cr_qp_offset: i8,
    pub sao_luma: bool,
    pub sao_chroma: bool,
}

pub struct PicState {
    pub width: usize,
    pub height: usize,
    /// Size in 4×4 units.
    pub w4: usize,
    pub h4: usize,
    pub log2_ctb: usize,
    pub ctb_w: usize,
    pub ctb_h: usize,
    /// `MinTbAddrZs` at 4×4 granularity.
    pub zs: Vec<u32>,
    /// Per CTB (raster): `SliceAddrRs` of the slice containing it, or -1.
    pub slice_addr: Vec<i32>,
    /// Per CTB (raster): index into the picture's slice header list.
    pub ctb_slice: Vec<u16>,
    /// Per CTB (raster): tile id.
    pub tile_id: Vec<u32>,
    /// Per 4×4: `CuPredMode` (PRED_*).
    pub pred_mode: Vec<u8>,
    /// Per 4×4: luma intra prediction mode.
    pub intra_mode: Vec<u8>,
    /// Per 4×4: `QpY`.
    pub qp_y: Vec<i8>,
    /// Per 4×4: coding quadtree depth (split_cu_flag context).
    pub ct_depth: Vec<u8>,
    /// Per 4×4: bit0 = left edge is a TU edge, bit1 = top edge is a TU edge,
    /// bit2 = left edge is a PU edge, bit3 = top edge is a PU edge.
    pub edges: Vec<u8>,
    /// Per 4×4: luma transform block has non-zero coefficients.
    pub nz: Vec<u8>,
    /// Per 4×4: samples must not be touched by the loop filters
    /// (`pcm_loop_filter_disabled_flag && pcm_flag`, or `cu_transquant_bypass_flag`).
    pub filter_bypass: Vec<u8>,
    /// Per 4×4: motion.
    pub motion: Vec<Motion>,
    /// Per CTB: SAO parameters for Y, Cb, Cr.
    pub sao: Vec<[SaoParams; 3]>,
    /// Per CTB: the slice-level filter parameters that apply to it.
    pub ctb_filter: Vec<CtbFilterParams>,
}

impl PicState {
    pub fn new(sps: &Sps, tiles: &TileLayout) -> Self {
        let width = sps.width as usize;
        let height = sps.height as usize;
        let w4 = width.div_ceil(4);
        let h4 = height.div_ceil(4);
        let log2_ctb = sps.log2_ctb_size as usize;
        let ctb_w = sps.pic_width_in_ctbs as usize;
        let ctb_h = sps.pic_height_in_ctbs as usize;
        let n4 = w4 * h4;
        let nctb = ctb_w * ctb_h;
        // (6-10) at 4×4 granularity
        let mut zs = vec![0u32; n4];
        let shift = log2_ctb - 2;
        for y in 0..h4 {
            for x in 0..w4 {
                let tb_x = x >> shift;
                let tb_y = y >> shift;
                let ctb_rs = ctb_w * tb_y + tb_x;
                let mut v = tiles.rs_to_ts[ctb_rs] << (shift * 2);
                for i in 0..shift {
                    let m = 1usize << i;
                    if m & x != 0 {
                        v += (m * m) as u32;
                    }
                    if m & y != 0 {
                        v += (2 * m * m) as u32;
                    }
                }
                zs[y * w4 + x] = v;
            }
        }
        let mut tile_id = vec![0u32; nctb];
        for (rs, t) in tile_id.iter_mut().enumerate() {
            *t = tiles.tile_id[tiles.rs_to_ts[rs] as usize];
        }
        PicState {
            width,
            height,
            w4,
            h4,
            log2_ctb,
            ctb_w,
            ctb_h,
            zs,
            slice_addr: vec![-1; nctb],
            ctb_slice: vec![0; nctb],
            tile_id,
            pred_mode: vec![PRED_NONE; n4],
            intra_mode: vec![1; n4],
            qp_y: vec![0; n4],
            ct_depth: vec![0; n4],
            edges: vec![0; n4],
            nz: vec![0; n4],
            filter_bypass: vec![0; n4],
            motion: vec![Motion::default(); n4],
            sao: vec![[SaoParams::default(); 3]; nctb],
            ctb_filter: vec![CtbFilterParams::default(); nctb],
        }
    }

    #[inline]
    pub fn idx4(&self, x: usize, y: usize) -> usize {
        (y >> 2) * self.w4 + (x >> 2)
    }

    #[inline]
    pub fn ctb_of(&self, x: usize, y: usize) -> usize {
        (y >> self.log2_ctb) * self.ctb_w + (x >> self.log2_ctb)
    }

    /// §6.4.1: is the luma location (xn, yn) available for the block at (xc, yc)?
    #[inline]
    pub fn available(&self, xc: i32, yc: i32, xn: i32, yn: i32) -> bool {
        if xn < 0 || yn < 0 || xn >= self.width as i32 || yn >= self.height as i32 {
            return false;
        }
        let (xn, yn, xc, yc) = (xn as usize, yn as usize, xc as usize, yc as usize);
        if self.zs[self.idx4(xn, yn)] > self.zs[self.idx4(xc, yc)] {
            return false;
        }
        let cn = self.ctb_of(xn, yn);
        let cc = self.ctb_of(xc, yc);
        if self.slice_addr[cn] < 0 || self.slice_addr[cn] != self.slice_addr[cc] {
            return false;
        }
        if self.tile_id[cn] != self.tile_id[cc] {
            return false;
        }
        // Decoded at all (a lost slice leaves PRED_NONE).
        self.pred_mode[self.idx4(xn, yn)] != PRED_NONE
    }

    /// Fills a rectangle (luma sample units) of a per-4×4 map.
    pub fn fill4<T: Copy>(map: &mut [T], w4: usize, x: usize, y: usize, w: usize, h: usize, v: T) {
        let x0 = x >> 2;
        let y0 = y >> 2;
        let x1 = (x + w).div_ceil(4);
        let y1 = (y + h).div_ceil(4);
        for yy in y0..y1 {
            for xx in x0..x1 {
                map[yy * w4 + xx] = v;
            }
        }
    }
}
