//! Slice segment data (§7.3.8): the CTU loop with tile/WPP substreams and
//! context storage/synchronisation (§9.3.1–9.3.2), SAO syntax, the coding
//! quadtree, coding units and prediction units. The transform tree,
//! residual coding and reconstruction live in the `residual` child module.
//!
//! Inter prediction units are parsed (so P/B slices never desync) but not
//! yet motion-compensated: that is Phase 3.

mod inter;
mod residual;

use crate::bits::BitReader;
use crate::cabac::*;
use crate::decoder::{RefLists, RefPic};
use crate::error::{Error, Result};
use crate::frame::Picture;
use crate::nal::Rbsp;
use crate::pic::{CtbFilterParams, PicState, SaoParams, PRED_INTER, PRED_INTRA, PRED_SKIP};
use crate::ps::{Pps, ScalingList, Sps, TileLayout};
use crate::slice::{SliceHeader, SliceType};
use crate::tables::{DIAG_4X4, DIAG_8X8};

/// `part_mode` (Table 7-10).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PartMode {
    Part2Nx2N,
    Part2NxN,
    PartNx2N,
    PartNxN,
    Part2NxnU,
    Part2NxnD,
    PartnLx2N,
    PartnRx2N,
}

/// Context state shared by the slice segments of one picture (WPP storage
/// and the dependent-slice-segment storage, §9.3.2.3/9.3.2.4).
#[derive(Default)]
pub struct CtxStore {
    pub wpp: Option<Contexts>,
    pub ds: Option<Contexts>,
    /// `QpY` of the last coding unit of the previous slice segment: a
    /// dependent slice segment continues the same *slice*, so §8.6.1's
    /// "first quantization group in a slice" reset does not apply to it.
    pub ds_qp: Option<i32>,
}

/// `ScalingFactor` (§7.4.5) as raster N×N tables: `[sizeId][matrixId]`.
pub struct ScalingFactors {
    pub f: Vec<Vec<Vec<u8>>>,
}

impl ScalingFactors {
    pub fn new(sl: &ScalingList) -> Self {
        let mut f = vec![vec![Vec::new(); 6]; 4];
        for size_id in 0..4usize {
            let n = 4usize << size_id;
            for matrix_id in 0..6usize {
                let mut t = vec![16u8; n * n];
                let list = &sl.lists[size_id][matrix_id];
                match size_id {
                    0 => {
                        for (i, &(x, y)) in DIAG_4X4.iter().enumerate() {
                            t[y as usize * 4 + x as usize] = list[i];
                        }
                    }
                    1 => {
                        for (i, &(x, y)) in DIAG_8X8.iter().enumerate() {
                            t[y as usize * 8 + x as usize] = list[i];
                        }
                    }
                    _ => {
                        let rep = n / 8;
                        for (i, &(x, y)) in DIAG_8X8.iter().enumerate() {
                            for j in 0..rep {
                                for k in 0..rep {
                                    t[(y as usize * rep + j) * n + x as usize * rep + k] = list[i];
                                }
                            }
                        }
                        t[0] = sl.dc[size_id - 2][matrix_id];
                    }
                }
                f[size_id][matrix_id] = t;
            }
        }
        ScalingFactors { f }
    }
}

pub struct SliceDecoder<'a> {
    sps: &'a Sps,
    pps: &'a Pps,
    sh: &'a SliceHeader,
    tiles: &'a TileLayout,
    pic: &'a mut Picture,
    st: &'a mut PicState,
    store: &'a mut CtxStore,
    rbsp: &'a Rbsp,
    cab: Cabac<'a>,
    scaling: Option<&'a ScalingFactors>,
    refs: &'a RefLists,
    poc: i32,
    /// `NoBackwardPredFlag`: every reference precedes the current picture.
    no_backward_pred: bool,
    /// The collocated picture for TMVP.
    col: Option<RefPic>,
    slice_addr_rs: i32,
    slice_idx: u16,
    init_type: usize,
    /// Substream start offsets in the RBSP (from the entry points).
    substreams: Vec<usize>,
    substream_idx: usize,
    // QP state (§8.6.1)
    qp_y: i32,
    qp_y_pred: i32,
    last_cu_qp: i32,
    qp_prev_reset: bool,
    is_cu_qp_delta_coded: bool,
    cu_qp_delta_val: i32,
    // current CU
    cu_x: usize,
    cu_y: usize,
    cu_size: usize,
    cu_transquant_bypass: bool,
    cu_intra: bool,
    part_mode: PartMode,
    intra_split: bool,
    max_trafo_depth: u8,
    intra_chroma_mode: u8,
    coeffs: Vec<i32>,
    /// Stage-1 scratch for the inverse transform, reused across blocks.
    itx_tmp: Vec<i32>,
    /// Bring-up traces (`RH265_DBG_CU` / `RH265_DBG_SUB`), read once per slice.
    dbg_cu: bool,
    dbg_sub: bool,
    /// Stage ablation for the ceiling probes (`codec-measurement`): parse
    /// everything, skip one pixel stage. Read once per slice so the hot loops
    /// see a plain bool, never an env lookup or an atomic.
    pub(crate) ablate: Ablate,
    /// Reused interpolation buffers — see `mcscratch`.
    pub(crate) scratch: crate::mcscratch::McScratch,
    /// Intra reference samples, reused across blocks. Allocating this per
    /// block zeroed ~388 bytes each time — for a 4x4 block that is 24 bytes
    /// of memset per predicted sample.
    pub(crate) iref: crate::intra::RefSamples,
    /// Reciprocal of `ctb_w`, so the CTB loop needs no hardware divide.
    ctb_div: CtbDiv,
}

/// Which pixel stages to skip. Parsing is never ablated — HEVC's entropy
/// decode does not depend on reconstructed samples, so every arm decodes the
/// same bins and the work counts stay comparable.
#[derive(Debug, Clone, Copy, Default)]
pub struct Ablate {
    /// `RH265_ABLATE_MC`: skip motion compensation.
    pub mc: bool,
    /// `RH265_ABLATE_INTRA`: skip intra prediction.
    pub intra: bool,
    /// `RH265_ABLATE_RESIDUAL`: parse coefficients, skip scaling / inverse
    /// transform / residual add.
    pub residual: bool,
}

impl Ablate {
    pub fn from_env() -> Ablate {
        Ablate {
            mc: std::env::var_os("RH265_ABLATE_MC").is_some(),
            intra: std::env::var_os("RH265_ABLATE_INTRA").is_some(),
            residual: std::env::var_os("RH265_ABLATE_RESIDUAL").is_some(),
        }
    }

    pub fn any(&self) -> bool {
        self.mc || self.intra || self.residual
    }
}

/// One-time inputs to build a [`SliceDecoder`].
pub struct SliceInputs<'a> {
    pub sps: &'a Sps,
    pub pps: &'a Pps,
    pub sh: &'a SliceHeader,
    pub tiles: &'a TileLayout,
    pub pic: &'a mut Picture,
    pub st: &'a mut PicState,
    pub store: &'a mut CtxStore,
    pub rbsp: &'a Rbsp,
    pub scaling: Option<&'a ScalingFactors>,
    pub refs: &'a RefLists,
    pub poc: i32,
    pub ablate: Ablate,
    pub slice_addr_rs: i32,
    pub slice_idx: u16,
}

/// Exact division by a value that is constant for the whole picture.
///
/// The coding-tree-block loop is full of `rs / ctb_w` and `rs % ctb_w`. `ctb_w`
/// is fixed for the picture but is not a compile-time constant, so each one
/// emitted a hardware `idiv` — 20-40 cycles, not pipelined, and the one integer
/// operation x86 has no SIMD form of. It is the integer twin of a libm call in
/// a float loop, and it goes the same way: replace it with arithmetic.
///
/// With `m = ceil(2^S / d)`, `(n · m) >> S == n / d` exactly for every
/// `n < 2^S / d`. `S = 40` against at most 2^18 coding tree blocks and a
/// `ctb_w` under 2^11 leaves eleven bits of headroom, and `n · m` stays inside
/// `u64`. The `debug_assert`s check every use against the real division.
#[derive(Clone, Copy)]
pub(crate) struct CtbDiv {
    d: usize,
    m: u64,
}

impl CtbDiv {
    const S: u32 = 40;

    pub(crate) fn new(d: usize) -> Self {
        debug_assert!(d > 0);
        let m = ((1u128 << Self::S) + d as u128 - 1) / d as u128;
        CtbDiv { d, m: m as u64 }
    }

    #[inline]
    pub(crate) fn div(self, n: usize) -> usize {
        let q = (((n as u64) * self.m) >> Self::S) as usize;
        debug_assert_eq!(q, n / self.d, "CtbDiv::div({n}) with d={}", self.d);
        q
    }

    #[inline]
    pub(crate) fn rem(self, n: usize) -> usize {
        let r = n - self.div(n) * self.d;
        debug_assert_eq!(r, n % self.d, "CtbDiv::rem({n}) with d={}", self.d);
        r
    }
}

pub(crate) fn wrap_qp(v: i32, qp_bd_offset: i32) -> i32 {
    ((v + 52 + 2 * qp_bd_offset) % (52 + qp_bd_offset)) - qp_bd_offset
}

impl<'a> SliceDecoder<'a> {
    pub fn new(inp: SliceInputs<'a>) -> Result<Self> {
        let sh = inp.sh;
        // Taken before `inp` is consumed below.
        let ctb_div = CtbDiv::new(inp.st.ctb_w.max(1));
        let init_type = match sh.slice_type {
            SliceType::I => 0,
            SliceType::P => {
                if sh.cabac_init_flag {
                    2
                } else {
                    1
                }
            }
            SliceType::B => {
                if sh.cabac_init_flag {
                    1
                } else {
                    2
                }
            }
        };
        // Entry points count escaped bytes from the first byte of slice data.
        let mut substreams = Vec::with_capacity(sh.entry_point_offsets.len() + 1);
        substreams.push(sh.data_offset);
        let rbsp = inp.rbsp;
        let mut esc = {
            // RBSP offset → escaped offset
            let mut e = sh.data_offset;
            for &p in &rbsp.epb_pos {
                if p <= e {
                    e += 1;
                } else {
                    break;
                }
            }
            e
        };
        for &off in &sh.entry_point_offsets {
            esc += off as usize;
            let r = rbsp.escaped_to_rbsp(esc);
            if r > rbsp.data.len() {
                return Err(Error::invalid("entry point beyond the NAL"));
            }
            substreams.push(r);
        }
        let cab = Cabac::new(&rbsp.data, sh.data_offset, Contexts::init(init_type, sh.slice_qp));
        let ds_qp = inp.store.ds_qp;
        let refs = inp.refs;
        let no_backward_pred = refs.l0.iter().chain(refs.l1.iter()).all(|r| r.poc <= inp.poc);
        let col = if sh.temporal_mvp_enabled && !sh.slice_type.is_intra() {
            let list = if sh.slice_type.is_b() && !sh.collocated_from_l0 { &refs.l1 } else { &refs.l0 };
            list.get(sh.collocated_ref_idx as usize).cloned()
        } else {
            None
        };
        Ok(SliceDecoder {
            sps: inp.sps,
            pps: inp.pps,
            sh,
            tiles: inp.tiles,
            pic: inp.pic,
            st: inp.st,
            store: inp.store,
            rbsp,
            cab,
            scaling: inp.scaling,
            refs,
            poc: inp.poc,
            no_backward_pred,
            col,
            slice_addr_rs: inp.slice_addr_rs,
            slice_idx: inp.slice_idx,
            init_type,
            substreams,
            substream_idx: 0,
            qp_y: sh.slice_qp,
            qp_y_pred: sh.slice_qp,
            last_cu_qp: if sh.dependent_slice_segment { ds_qp.unwrap_or(sh.slice_qp) } else { sh.slice_qp },
            qp_prev_reset: !sh.dependent_slice_segment,
            is_cu_qp_delta_coded: false,
            cu_qp_delta_val: 0,
            cu_x: 0,
            cu_y: 0,
            cu_size: 0,
            cu_transquant_bypass: false,
            cu_intra: true,
            part_mode: PartMode::Part2Nx2N,
            intra_split: false,
            max_trafo_depth: 0,
            intra_chroma_mode: 0,
            coeffs: vec![0; 32 * 32],
            itx_tmp: vec![0; 32 * 32],
            dbg_cu: std::env::var_os("RH265_DBG_CU").is_some(),
            dbg_sub: std::env::var_os("RH265_DBG_SUB").is_some(),
            ablate: inp.ablate,
            scratch: crate::mcscratch::McScratch::new(),
            iref: crate::intra::RefSamples::new(),
            ctb_div,
        })
    }

    fn first_in_tile(&self, ts: usize) -> bool {
        ts == 0 || self.tiles.tile_id[ts] != self.tiles.tile_id[ts - 1]
    }

    /// Width in CTBs of the tile containing the CTB at raster address `rs`.
    fn tile_width_ctus(&self, rs: usize) -> u32 {
        let cx = self.ctb_div.rem(rs) as u32;
        for i in 0..self.tiles.col_bd.len() - 1 {
            if cx >= self.tiles.col_bd[i] && cx < self.tiles.col_bd[i + 1] {
                return self.tiles.col_bd[i + 1] - self.tiles.col_bd[i];
            }
        }
        self.st.ctb_w as u32
    }

    /// First CTB of a CTB row inside its tile.
    fn row_start(&self, rs: usize, ts: usize) -> bool {
        self.ctb_div.rem(rs) == 0 || (ts > 0 && self.tiles.tile_id[ts] != self.tiles.tile_id[self.tiles.rs_to_ts[rs - 1] as usize])
    }

    /// §7.3.8.1 slice_segment_data(): decodes every CTU of the segment.
    pub fn decode(mut self) -> Result<()> {
        let _w = self.st.ctb_w;
        let mut rs = self.sh.segment_address as usize;
        let mut ts = self.tiles.rs_to_ts[rs] as usize;
        let wpp = self.pps.entropy_coding_sync_enabled;
        // §9.3.1: a dependent slice segment continues the previous segment's
        // context variables — unless it starts at the first CTB of a tile
        // (that CTB initialises), or the tile is a single CTB wide under WPP
        // (where the wavefront storage takes over instead).
        if self.sh.dependent_slice_segment && !self.first_in_tile(ts) && (self.tile_width_ctus(rs) >= 2 || !wpp) {
            if let Some(c) = &self.store.ds {
                self.cab.ctx = c.clone();
            }
        }
        let mut segment_start = true;
        loop {
            // Context initialisation / synchronisation at the CTU start (§9.3.1).
            self.st.slice_addr[rs] = self.slice_addr_rs;
            let x0 = self.ctb_div.rem(rs) << self.st.log2_ctb;
            let y0 = self.ctb_div.div(rs) << self.st.log2_ctb;
            if self.first_in_tile(ts) {
                if !segment_start {
                    self.cab.ctx = Contexts::init(self.init_type, self.sh.slice_qp);
                }
                self.qp_prev_reset = true;
            } else if wpp && self.row_start(rs, ts) {
                if !segment_start {
                    self.cab.ctx = Contexts::init(self.init_type, self.sh.slice_qp);
                }
                let ctb = 1i32 << self.st.log2_ctb;
                if self.st.available(x0 as i32, y0 as i32, x0 as i32 + ctb, y0 as i32 - ctb) {
                    if let Some(c) = &self.store.wpp {
                        self.cab.ctx = c.clone();
                    }
                }
                self.qp_prev_reset = true;
            }
            segment_start = false;

            self.decode_ctu(rs, x0, y0)?;

            // WPP storage after the second CTB of a row (§9.3.2.3 trigger).
            if wpp && (self.ctb_div.rem(rs) == 1 || (rs > 1 && self.tiles.tile_id[ts] != self.tiles.tile_id[self.tiles.rs_to_ts[rs - 2] as usize])) {
                self.store.wpp = Some(self.cab.ctx.clone());
            }
            let end_of_slice_segment = self.cab.terminate();
            ts += 1;
            if end_of_slice_segment {
                if self.pps.dependent_slice_segments_enabled {
                    self.store.ds = Some(self.cab.ctx.clone());
                    self.store.ds_qp = Some(self.last_cu_qp);
                }
                return Ok(());
            }
            if ts >= self.tiles.ts_to_rs.len() {
                return Err(Error::invalid("slice segment runs past the last CTB"));
            }
            rs = self.tiles.ts_to_rs[ts] as usize;
            let new_tile = self.pps.tiles_enabled && self.first_in_tile(ts);
            let new_row = wpp && self.row_start(rs, ts);
            if new_tile || new_row {
                if !self.cab.terminate() {
                    return Err(Error::invalid("end_of_subset_one_bit is 0"));
                }
                self.substream_idx += 1;
                let natural = self.cab.aligned_byte_pos();
                let start = match self.substreams.get(self.substream_idx) {
                    Some(&s) => s,
                    None => natural,
                };
                if self.dbg_sub {
                    eprintln!(
                        "sub poc={} next_rs={rs} idx={} entry_start={start} natural={natural} nsub={}",
                        self.poc, self.substream_idx, self.substreams.len()
                    );
                }
                self.cab.reinit_at(start);
            }
        }
    }

    fn decode_ctu(&mut self, rs: usize, x0: usize, y0: usize) -> Result<()> {
        self.st.ctb_slice[rs] = self.slice_idx;
        self.st.ctb_filter[rs] = CtbFilterParams {
            deblock_disabled: self.sh.deblocking_filter_disabled,
            beta_offset_div2: self.sh.beta_offset_div2 as i8,
            tc_offset_div2: self.sh.tc_offset_div2 as i8,
            lf_across_slices: self.sh.loop_filter_across_slices_enabled,
            cb_qp_offset: (self.pps.cb_qp_offset + self.sh.cb_qp_offset) as i8,
            cr_qp_offset: (self.pps.cr_qp_offset + self.sh.cr_qp_offset) as i8,
            sao_luma: self.sh.sao_luma,
            sao_chroma: self.sh.sao_chroma,
        };
        if self.sh.sao_luma || self.sh.sao_chroma {
            self.parse_sao(rs)?;
        } else {
            self.st.sao[rs] = [SaoParams::default(); 3];
        }
        self.coding_quadtree(x0, y0, self.sps.log2_ctb_size as usize, 0)
    }

    // ---- SAO syntax (§7.3.8.3) ----

    fn parse_sao(&mut self, rs: usize) -> Result<()> {
        let w = self.st.ctb_w;
        let rx = self.ctb_div.rem(rs);
        let ry = self.ctb_div.div(rs);
        let mut merge_left = false;
        let mut merge_up = false;
        if rx > 0 {
            let left_in_slice = rs as i32 > self.slice_addr_rs;
            let left_in_tile = self.st.tile_id[rs] == self.st.tile_id[rs - 1];
            if left_in_slice && left_in_tile {
                merge_left = self.cab.decode(CTX_SAO_MERGE) == 1;
            }
        }
        if ry > 0 && !merge_left {
            let up_in_slice = (rs - w) as i32 >= self.slice_addr_rs;
            let up_in_tile = self.st.tile_id[rs] == self.st.tile_id[rs - w];
            if up_in_slice && up_in_tile {
                merge_up = self.cab.decode(CTX_SAO_MERGE) == 1;
            }
        }
        if merge_left {
            self.st.sao[rs] = self.st.sao[rs - 1];
            return Ok(());
        }
        if merge_up {
            self.st.sao[rs] = self.st.sao[rs - w];
            return Ok(());
        }
        let mut params = [SaoParams::default(); 3];
        for c in 0..3usize {
            let enabled = if c == 0 { self.sh.sao_luma } else { self.sh.sao_chroma };
            if !enabled {
                continue;
            }
            let type_idx = if c < 2 {
                if self.cab.decode(CTX_SAO_TYPE) == 0 {
                    0
                } else if self.cab.bypass() == 0 {
                    1
                } else {
                    2
                }
            } else {
                params[1].type_idx
            };
            params[c].type_idx = type_idx;
            if type_idx == 0 {
                continue;
            }
            let bit_depth = if c == 0 { self.sps.bit_depth_luma } else { self.sps.bit_depth_chroma };
            let cmax = (1u32 << (bit_depth.min(10) - 5)) - 1;
            let mut abs = [0i32; 4];
            for a in abs.iter_mut() {
                let mut v = 0;
                while v < cmax && self.cab.bypass() == 1 {
                    v += 1;
                }
                *a = v as i32;
            }
            let shift = bit_depth - bit_depth.min(10);
            if type_idx == 1 {
                for a in abs.iter_mut() {
                    if *a != 0 && self.cab.bypass() == 1 {
                        *a = -*a;
                    }
                }
                params[c].aux = self.cab.bypass_bits(5) as u8;
                for i in 0..4 {
                    params[c].offset[i] = (abs[i] << shift) as i16;
                }
            } else {
                // §7.3.8.3: `sao_eo_class_luma` (cIdx 0) and
                // `sao_eo_class_chroma` (cIdx 1) are different syntax elements
                // that happen to share a binarisation, and cIdx 2 does not code
                // a class at all -- it reuses cIdx 1's.
                params[c].aux = if c < 2 { self.cab.bypass_bits(2) as u8 } else { params[1].aux };
                params[c].offset = [(abs[0] << shift) as i16, (abs[1] << shift) as i16, (-(abs[2] << shift)) as i16, (-(abs[3] << shift)) as i16];
            }
        }
        self.st.sao[rs] = params;
        Ok(())
    }
    // ---- coding quadtree (§7.3.8.4) ----

    fn coding_quadtree(&mut self, x0: usize, y0: usize, log2cb: usize, depth: u8) -> Result<()> {
        let size = 1usize << log2cb;
        let w = self.st.width;
        let h = self.st.height;
        let min_cb = self.sps.log2_min_cb_size as usize;
        let split = if x0 + size <= w && y0 + size <= h && log2cb > min_cb {
            let xi = x0 as i32;
            let yi = y0 as i32;
            let cond_l = self.st.available(xi, yi, xi - 1, yi) && self.st.ct_depth[self.st.idx4(x0 - 1, y0)] > depth;
            let cond_a = self.st.available(xi, yi, xi, yi - 1) && self.st.ct_depth[self.st.idx4(x0, y0 - 1)] > depth;
            self.cab.decode(CTX_SPLIT_CU + cond_l as usize + cond_a as usize) == 1
        } else {
            log2cb > min_cb
        };
        // §7.4.9.4 / §8.6.1: a quantization group is a coding-quadtree node of
        // exactly `Log2MinCuQpDeltaSize`, or a coding unit larger than that.
        // Starting one at every larger *ancestor* node would consume the
        // slice/tile/wavefront reset of qPY_PREV before the group that owns
        // it, and then re-predict from the previous CTB's last CU.
        let log2_min_qg = self.sps.log2_ctb_size as usize - self.pps.diff_cu_qp_delta_depth as usize;
        if log2cb == log2_min_qg || (log2cb > log2_min_qg && !split) {
            // A new quantization group (§8.6.1).
            if self.pps.cu_qp_delta_enabled {
                self.is_cu_qp_delta_coded = false;
                self.cu_qp_delta_val = 0;
            }
            let qp_prev = if self.qp_prev_reset { self.sh.slice_qp } else { self.last_cu_qp };
            self.qp_prev_reset = false;
            let xi = x0 as i32;
            let yi = y0 as i32;
            let cur_ctb = self.st.ctb_of(x0, y0);
            let qp_a = if self.st.available(xi, yi, xi - 1, yi) && self.st.ctb_of(x0 - 1, y0) == cur_ctb {
                self.st.qp_y[self.st.idx4(x0 - 1, y0)] as i32
            } else {
                qp_prev
            };
            let qp_b = if self.st.available(xi, yi, xi, yi - 1) && self.st.ctb_of(x0, y0 - 1) == cur_ctb {
                self.st.qp_y[self.st.idx4(x0, y0 - 1)] as i32
            } else {
                qp_prev
            };
            self.qp_y_pred = (qp_a + qp_b + 1) >> 1;
        }
        if split {
            let half = size / 2;
            for (dx, dy) in [(0, 0), (half, 0), (0, half), (half, half)] {
                if x0 + dx < w && y0 + dy < h {
                    self.coding_quadtree(x0 + dx, y0 + dy, log2cb - 1, depth + 1)?;
                }
            }
            Ok(())
        } else {
            self.coding_unit(x0, y0, log2cb, depth)
        }
    }

    fn set_cu_qp(&mut self) {
        let w4 = self.st.w4;
        PicState::fill4(&mut self.st.qp_y, w4, self.cu_x, self.cu_y, self.cu_size, self.cu_size, self.qp_y as i8);
    }

    /// Marks edges on the left and top sides of a block: `kind` 0 = transform
    /// block edge (bit0/1), 1 = prediction block edge (bit2/3), 2 = coding
    /// block edge (both). An internal PU edge is *not* a transform edge
    /// (§8.7.2.3): the cbf rule of the boundary strength must not see it.
    fn mark_edges(&mut self, x: usize, y: usize, w: usize, h: usize, kind: u8) {
        let (bits, bits_h) = match kind {
            0 => (0b0001, 0b0010),
            1 => (0b0100, 0b1000),
            _ => (0b0101, 0b1010),
        };
        let w4 = self.st.w4;
        for yy in (y >> 2)..((y + h) >> 2) {
            self.st.edges[yy * w4 + (x >> 2)] |= bits;
        }
        for xx in (x >> 2)..((x + w) >> 2) {
            self.st.edges[(y >> 2) * w4 + xx] |= bits_h;
        }
    }

    // ---- coding unit (§7.3.8.5) ----

    fn coding_unit(&mut self, x0: usize, y0: usize, log2cb: usize, depth: u8) -> Result<()> {
        let n = 1usize << log2cb;
        let w4 = self.st.w4;
        self.cu_x = x0;
        self.cu_y = y0;
        self.cu_size = n;
        self.cu_transquant_bypass = self.pps.transquant_bypass_enabled && self.cab.decode(CTX_TRANSQUANT_BYPASS) == 1;
        let xi = x0 as i32;
        let yi = y0 as i32;
        let mut skip = false;
        if !self.sh.slice_type.is_intra() {
            let cond_l = self.st.available(xi, yi, xi - 1, yi) && self.st.pred_mode[self.st.idx4(x0 - 1, y0)] == PRED_SKIP;
            let cond_a = self.st.available(xi, yi, xi, yi - 1) && self.st.pred_mode[self.st.idx4(x0, y0 - 1)] == PRED_SKIP;
            skip = self.cab.decode(CTX_CU_SKIP + cond_l as usize + cond_a as usize) == 1;
        }
        self.qp_y = wrap_qp(self.qp_y_pred + self.cu_qp_delta_val, self.sps.qp_bd_offset_y);
        PicState::fill4(&mut self.st.ct_depth, w4, x0, y0, n, n, depth);
        self.set_cu_qp();
        PicState::fill4(&mut self.st.filter_bypass, w4, x0, y0, n, n, self.cu_transquant_bypass as u8);
        self.mark_edges(x0, y0, n, n, 2);

        if skip {
            PicState::fill4(&mut self.st.pred_mode, w4, x0, y0, n, n, PRED_SKIP);
            PicState::fill4(&mut self.st.intra_mode, w4, x0, y0, n, n, 1);
            self.cu_intra = false;
            self.part_mode = PartMode::Part2Nx2N;
            self.prediction_unit(x0, y0, n, x0, y0, n, n, 0, depth, true)?;
            self.last_cu_qp = self.qp_y;
            self.dbg_cu(x0, y0, n);
            return Ok(());
        }
        self.cu_intra = if self.sh.slice_type.is_intra() { true } else { self.cab.decode(CTX_PRED_MODE) == 1 };
        let min_cb = self.sps.log2_min_cb_size as usize;
        self.part_mode = PartMode::Part2Nx2N;
        if !self.cu_intra || log2cb == min_cb {
            self.part_mode = self.parse_part_mode(log2cb)?;
        }
        PicState::fill4(&mut self.st.pred_mode, w4, x0, y0, n, n, if self.cu_intra { PRED_INTRA } else { PRED_INTER });
        self.intra_split = self.cu_intra && self.part_mode == PartMode::PartNxN;
        let mut pcm = false;
        let mut merge_2nx2n = false;
        if self.cu_intra {
            if self.part_mode == PartMode::Part2Nx2N
                && self.sps.pcm_enabled
                && log2cb >= self.sps.log2_min_pcm_cb_size as usize
                && log2cb <= self.sps.log2_max_pcm_cb_size as usize
            {
                pcm = self.cab.terminate();
            }
            if pcm {
                PicState::fill4(&mut self.st.intra_mode, w4, x0, y0, n, n, 1);
                if self.sps.pcm_loop_filter_disabled {
                    PicState::fill4(&mut self.st.filter_bypass, w4, x0, y0, n, n, 1);
                }
                self.pcm_samples(x0, y0, n)?;
            } else {
                self.parse_intra_modes(x0, y0, n)?;
            }
        } else {
            merge_2nx2n = self.inter_prediction_units(x0, y0, n, depth)?;
            PicState::fill4(&mut self.st.intra_mode, w4, x0, y0, n, n, 1);
        }
        if !pcm {
            let mut rqt_root_cbf = true;
            if !self.cu_intra && !(self.part_mode == PartMode::Part2Nx2N && merge_2nx2n) {
                rqt_root_cbf = self.cab.decode(CTX_RQT_ROOT_CBF) == 1;
            }
            if rqt_root_cbf {
                self.max_trafo_depth = if self.cu_intra {
                    self.sps.max_transform_hierarchy_depth_intra + self.intra_split as u8
                } else {
                    self.sps.max_transform_hierarchy_depth_inter
                };
                self.transform_tree(x0, y0, x0, y0, log2cb, 0, 0, true, true)?;
            }
        }
        self.last_cu_qp = self.qp_y;
        self.dbg_cu(x0, y0, n);
        Ok(())
    }


    /// `RH265_DBG_CU=1`: one line per coding unit, in HM's `HM_DBG_CU` format.
    fn dbg_cu(&self, x0: usize, y0: usize, n: usize) {
        if !self.dbg_cu {
            return;
        }
        let i = self.st.idx4(x0, y0);
        let part = self.part_mode as u8;
        if self.cu_intra {
            eprintln!("cu poc={} x={x0} y={y0} w={n} intra qp={} part={part} mode={}", self.poc, self.qp_y, self.st.intra_mode[i]);
        } else {
            let m = self.st.motion[i];
            eprintln!(
                "cu poc={} x={x0} y={y0} w={n} inter qp={} part={part} mv0={},{} r0={} mv1={},{} r1={}",
                self.poc, self.qp_y, m.mv[0][0], m.mv[0][1], m.ref_idx[0], m.mv[1][0], m.mv[1][1], m.ref_idx[1]
            );
        }
    }

    fn parse_part_mode(&mut self, log2cb: usize) -> Result<PartMode> {
        if self.cab.decode(CTX_PART_MODE) == 1 {
            return Ok(PartMode::Part2Nx2N);
        }
        if self.cu_intra {
            return Ok(PartMode::PartNxN);
        }
        let min_cb = self.sps.log2_min_cb_size as usize;
        if log2cb == min_cb {
            if self.cab.decode(CTX_PART_MODE + 1) == 1 {
                return Ok(PartMode::Part2NxN);
            }
            if log2cb == 3 {
                return Ok(PartMode::PartNx2N);
            }
            if self.cab.decode(CTX_PART_MODE + 2) == 1 {
                return Ok(PartMode::PartNx2N);
            }
            return Ok(PartMode::PartNxN);
        }
        if !self.sps.amp_enabled {
            return Ok(if self.cab.decode(CTX_PART_MODE + 1) == 1 { PartMode::Part2NxN } else { PartMode::PartNx2N });
        }
        if self.cab.decode(CTX_PART_MODE + 1) == 1 {
            if self.cab.decode(CTX_PART_MODE + 3) == 1 {
                return Ok(PartMode::Part2NxN);
            }
            return Ok(if self.cab.bypass() == 0 { PartMode::Part2NxnU } else { PartMode::Part2NxnD });
        }
        if self.cab.decode(CTX_PART_MODE + 3) == 1 {
            return Ok(PartMode::PartNx2N);
        }
        Ok(if self.cab.bypass() == 0 { PartMode::PartnLx2N } else { PartMode::PartnRx2N })
    }

    // ---- intra mode syntax (§7.3.8.5) and derivation (§8.4.2 / §8.4.3) ----

    fn parse_intra_modes(&mut self, x0: usize, y0: usize, n: usize) -> Result<()> {
        let parts = if self.part_mode == PartMode::PartNxN { 4 } else { 1 };
        let pb = if parts == 4 { n / 2 } else { n };
        let mut prev = [false; 4];
        for p in prev.iter_mut().take(parts) {
            *p = self.cab.decode(CTX_PREV_INTRA_LUMA_PRED) == 1;
        }
        let w4 = self.st.w4;
        for j in 0..parts {
            let xp = x0 + (j & 1) * pb;
            let yp = y0 + (j >> 1) * pb;
            let cand = self.mpm_candidates(xp, yp);
            let mode = if prev[j] {
                let idx = if self.cab.bypass() == 0 {
                    0
                } else if self.cab.bypass() == 0 {
                    1
                } else {
                    2
                };
                cand[idx]
            } else {
                let mut m = self.cab.bypass_bits(5) as u8;
                let mut c = cand;
                c.sort_unstable();
                for &cm in &c {
                    if m >= cm {
                        m += 1;
                    }
                }
                m
            };
            PicState::fill4(&mut self.st.intra_mode, w4, xp, yp, pb, pb, mode);
        }
        if parts == 4 {
            // PU edges of the NxN split.
            self.mark_edges(x0 + pb, y0, pb, n, 1);
            self.mark_edges(x0, y0 + pb, n, pb, 1);
        }
        // intra_chroma_pred_mode (once for 4:2:0)
        let icpm = if self.cab.decode(CTX_INTRA_CHROMA_PRED_MODE) == 0 { 4 } else { self.cab.bypass_bits(2) as u8 };
        let luma0 = self.st.intra_mode[self.st.idx4(x0, y0)];
        self.intra_chroma_mode = match icpm {
            4 => luma0,
            _ => {
                let m = [0u8, 26, 10, 1][icpm as usize];
                if m == luma0 {
                    34
                } else {
                    m
                }
            }
        };
        Ok(())
    }

    fn mpm_candidates(&self, xp: usize, yp: usize) -> [u8; 3] {
        let xi = xp as i32;
        let yi = yp as i32;
        let cand = |xn: i32, yn: i32, above: bool| -> u8 {
            if !self.st.available(xi, yi, xn, yn) {
                return 1;
            }
            let i = self.st.idx4(xn as usize, yn as usize);
            if self.st.pred_mode[i] != PRED_INTRA {
                return 1;
            }
            if above && yn < ((yi >> self.st.log2_ctb) << self.st.log2_ctb) {
                return 1;
            }
            self.st.intra_mode[i]
        };
        let a = cand(xi - 1, yi, false);
        let b = cand(xi, yi - 1, true);
        if a == b {
            if a < 2 {
                [0, 1, 26]
            } else {
                [a, 2 + ((a + 29) % 32), 2 + ((a - 2 + 1) % 32)]
            }
        } else {
            let c = if a != 0 && b != 0 {
                0
            } else if a != 1 && b != 1 {
                1
            } else {
                26
            };
            [a, b, c]
        }
    }

    // ---- PCM (§7.3.8.7) ----

    fn pcm_samples(&mut self, x0: usize, y0: usize, n: usize) -> Result<()> {
        let start = self.cab.aligned_byte_pos();
        let data = &self.rbsp.data;
        if start > data.len() {
            return Err(Error::invalid("pcm past end of NAL"));
        }
        let mut br = BitReader::new(&data[start..]);
        let sps = self.sps;
        let shift_y = sps.bit_depth_luma - sps.pcm_bit_depth_luma;
        let shift_c = sps.bit_depth_chroma - sps.pcm_bit_depth_chroma;
        {
            let pl = &mut self.pic.planes[0];
            for y in 0..n {
                for x in 0..n {
                    let v = br.read_bits(sps.pcm_bit_depth_luma as u32)?;
                    pl.set(x0 + x, y0 + y, (v << shift_y) as u16);
                }
            }
        }
        for c in 1..3 {
            let pl = &mut self.pic.planes[c];
            for y in 0..n / 2 {
                for x in 0..n / 2 {
                    let v = br.read_bits(sps.pcm_bit_depth_chroma as u32)?;
                    pl.set(x0 / 2 + x, y0 / 2 + y, (v << shift_c) as u16);
                }
            }
        }
        let used = br.bit_pos().div_ceil(8);
        self.cab.reinit_at(start + used);
        Ok(())
    }

    /// k-th order Exp-Golomb, bypass coded (§9.3.3.6).
    pub(super) fn eg_k(&mut self, mut k: u32) -> Result<u32> {
        let mut v = 0u32;
        while self.cab.bypass() == 1 {
            v += 1 << k;
            k += 1;
            if k > 31 {
                return Err(Error::invalid("EGk prefix too long"));
            }
        }
        Ok(v + self.cab.bypass_bits(k))
    }

}

#[cfg(test)]
mod ctb_div_tests {
    use super::CtbDiv;

    #[test]
    fn ctb_div_is_exact_over_every_picture_width() {
        // Every `ctb_w` a conforming stream can have: level 6.2 caps the width
        // at 16888 luma samples, so 1056 coding tree blocks at the 16x16
        // minimum. Checked against the real division at the boundaries and
        // across the range.
        for d in 1..=1056usize {
            let f = CtbDiv::new(d);
            for n in [0, 1, d - 1, d, d + 1, 2 * d - 1, 2 * d, 12_345, 139_263] {
                assert_eq!(f.div(n), n / d, "div n={n} d={d}");
                assert_eq!(f.rem(n), n % d, "rem n={n} d={d}");
            }
        }
    }
}
