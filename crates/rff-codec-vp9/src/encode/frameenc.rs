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

use std::cell::RefCell;

use super::adapt::{COEF_COUNT_SAT, COEF_MAX_UPDATE_FACTOR};
use super::bitwriter::{BitWriter, BoolEncoder};
use super::compressed::write_compressed_header;
use super::frame::{assemble_frame, assemble_tiles};
use super::header::write_uncompressed_header;
use super::intermode::{
    write_comp_inter, write_comp_ref, write_inter_mode, write_interp_filter, write_is_inter,
    write_single_ref, SWITCHABLE_INTERP_TREE,
};
use crate::decode::{comp_ref_context, reference_mode_context};
use super::mv::encode_mv;
use super::quantize::quantize;
use super::syntax::{
    write_intra_mode, write_partition, write_segment_id, write_selected_tx_size, write_skip,
};
use super::tokens::{coef_cost, cost_bit, encode_coefs, tree_bit_cost, RateTracker};

pub static DEDUP: [std::sync::atomic::AtomicU64; 3] =
    [const { std::sync::atomic::AtomicU64::new(0) }; 3]; // emit_lookups, hits, matches
use super::prof;
use super::transform::forward_transform;
use super::varpart::VarTree;
use super::varrd;
use crate::block::{
    kf_uv_mode_probs, kf_y_mode_probs, partition_plane_context, skip_context, subsize,
    tx_size_context, update_partition_context, ModeInfo, Mv, ALTREF_FRAME, BLOCK_4X4, BLOCK_8X8,
    GOLDEN_FRAME,
    INTRA_FRAME, INTRA_MODE_TREE, LAST_FRAME, NEARESTMV, NEARMV, NEWMV, NONE_FRAME, PARTITION_NONE,
    PARTITION_HORZ, PARTITION_SPLIT, PARTITION_TREE, PARTITION_VERT, ZEROMV,
};
use std::sync::atomic::AtomicU64;
// VP9_PROF: cumulative wall-clock per encode stage (µs), summed across frames.
static PROF_DECISION: AtomicU64 = AtomicU64::new(0);
static PROF_EMIT1: AtomicU64 = AtomicU64::new(0);
static PROF_EMIT2: AtomicU64 = AtomicU64::new(0);
static PROF_HDR: AtomicU64 = AtomicU64::new(0);
// VP9_IF_HARVEST: interp-filter ceiling probe. [0]=Σ residual SSE @ EIGHTTAP,
// [1]=Σ residual SSE @ per-block best filter, [2]=inter blocks, [3]=blocks a
// non-EIGHTTAP filter beat EIGHTTAP on. The per-block residual reduction ([0]−[1])/[0]
// is the upper bound on the switchable-filter win (before signaling cost).
static IF_HARVEST: [AtomicU64; 4] = [const { AtomicU64::new(0) }; 4];
// VP9_SUB8_PROBE: sub-8×8 compound ceiling. [0]=Σ single-ref best SAD, [1]=Σ min(single,
// compound) SAD, [2]=sub-blocks, [3]=sub-blocks compound beat single. ([0]−[1])/[0] bounds
// the sub-8×8 compound PREDICTION win (before comp_inter/comp_ref/second-MV signalling).
static SUB8_PROBE: [AtomicU64; 4] = [const { AtomicU64::new(0) }; 4];
// VP9_LFSEG_PROBE: per-segment loop-filter ceiling. [0]=Σ global-best-level luma SSE,
// [1]=Σ per-SB-oracle luma SSE (each 64×64 SB picks its own level), [2]=frames. ([0]−[1])/[0]
// is the UPPER bound on the spatial per-segment-lf win (before the seg-map signalling cost).
static LFSEG_PROBE: [AtomicU64; 3] = [const { AtomicU64::new(0) }; 3];
use crate::decode::{average_split_mvs, INTER_MODE_TREE};
use crate::geom_tables::{B_HEIGHT_LOG2, B_WIDTH_LOG2};
use crate::decode::{
    adapt_coef_probs, clamp_mv_umv, intra_inter_context, single_ref_p1, single_ref_p2,
    switchable_interp_context, uv_tx_size, FrameContext, FrameCounts,
};
use crate::geom_tables::{MAX_TXSIZE, SIZE_GROUP};
use crate::inter::{predict_block, RefPlane};
use crate::loopfilter::loop_filter_frame;
use crate::mv::{find_mv_refs, get_mode_context, lower_mv_precision, use_mv_hp, MvRef, NmvCounts};
use crate::predict::{build_intra_edges, predict};
use crate::prob_tables::{DEFAULT_COEF_PROBS, KF_PARTITION_PROBS};
use crate::quant::{ac_quant, dc_quant};
use crate::token::get_scan;
use crate::transform::{
    inv_basis_normsq_1d, inverse_transform_add_rows, inverse_transform_dc_add, TxType,
    INTRA_MODE_TO_TX_TYPE,
};
use crate::FrameHeader;

const MI_SIZE: usize = 8;
const BLOCK_64X64: usize = 12;
// Intra prediction modes (ISO/VP9 enum order).
const DC_PRED: u8 = 0;
const V_PRED: u8 = 1;
const H_PRED: u8 = 2;
const TM_PRED: u8 = 9;

/// AQ segment tree probs (2 segments): the path to leaves 0/1 is bits [0,0,·], so nodes
/// 0/1 (probs[0],[1]) are pinned to 0 (prob 255 ≈ free) and probs[3] = 128 splits seg0/seg1.
const AQ_TREE_PROBS: [u8; 7] = [255, 255, 255, 128, 255, 255, 255];
// NOTE: directional intra search (all 10 modes) was TRIED and REVERTED (2026-07-19).
// The decoder supports D45..D63 (315/315 vectors), but adding them to the ENCODER
// search was (a) NEUTRAL at best on a clean 4-clip corpus (−0.03% mean BD, bus LOST
// +2.10% — the search RD under-prices directional modes vs their real coded cost),
// and (b) the CHROMA path (best_intra_mode → uv_mode directional) DESYNCED on bus
// (ours 29.8 dB vs libvpx 18.8 dB) — the encoder's chroma intra recon diverges from
// spec for directional modes. A decoder supporting a tool ≠ the encoder search
// gaining from it. The real BD gap vs libvpx is compound prediction (see memory).

/// RDO trial snapshot of a luma block: `(pixels[row·bw+col] up to 64×64, above_ctx
/// footprint, whole left_ctx column)`.
type YSnap = (Vec<u16>, [u8; 16], [u8; 16]);

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
// (A thread-local rec-buffer POOL was tried to cut snap_block's alloc churn — WASH: SnapRestore
// stayed ~33ms, so it's COPY-dominated (Rust's allocator already recycles same-size Vecs), not
// alloc-dominated. The only lever is not copying the recon, which isn't byte-identical.)
//
// ...but that verdict was drawn from the `SnapRestore` bucket, which only spans the ALLOCATING
// half. A snapshot's seven `Vec`s are freed when it drops, and every drop site sits OUTSIDE any
// scope — so the `free()` half has always landed in the unscoped-orchestration residue. This
// `Drop` charges it explicitly. NOTE the ordering: the fields must be taken and dropped INSIDE
// the scope, because an implicit field drop would run AFTER `_sd` closes and leak right back
// into the parent (exactly the bug that inflated the AV1 profiler 3.45×).
// Buffer free-lists. A snapshot's seven `Vec`s are allocated in `snap_block` and freed
// when it drops; the sizes repeat endlessly (one per block size), so recycling them
// turns ~7 malloc/free pairs per snapshot into a pop/push of an already-sized buffer.
// Thread-local, so the per-tile decision workers each keep their own and never contend.
//
// VERIFIED (2026-07-26) as the byte-identical half of the snapshot cost: skipping the
// RECON copy instead (`VP9_NO_SNAP_RECON`) changes the bitstream on 8/8 gate streams, so
// the copy itself is load-bearing and only where its memory COMES FROM is free to change.
thread_local! {
    static POOL_U16: RefCell<Vec<Vec<u16>>> = const { RefCell::new(Vec::new()) };
    static POOL_U8: RefCell<Vec<Vec<u8>>> = const { RefCell::new(Vec::new()) };
    static POOL_MI: RefCell<Vec<Vec<ModeInfo>>> = const { RefCell::new(Vec::new()) };
}
/// The RD pass's decision cache: `(mi_row, mi_col, bsize) -> (winning mode, predictor,
/// tx size)`, written by the partition search and replayed by the emit pass.
///
/// It was a `std::collections::HashMap` keyed on a `(usize, usize, usize)` tuple — 24
/// bytes of key hashed with SipHash, measured at 102.3 ns/op and 12.5% of the encoder's
/// unscoped-orchestration residue. But the key is a dense bounded coordinate, so it packs
/// losslessly into one `u32` and needs no cryptographic hashing at all:
///
///   mi_row < 2^13, mi_col < 2^13, bsize < 2^5   ->   bsize<<26 | row<<13 | col
///
/// FIELD ORDER IS LOad-BEARING. FxHash mixes with a single multiply, which pushes entropy
/// toward the HIGH bits, while hashbrown picks its bucket from the LOW bits. Packing
/// `bsize` low (the original `row<<18 | col<<5 | bsize`) left the bottom 5 bits taking
/// only ~4 distinct values across a whole frame, so `key*SEED`'s low bits clustered and
/// the table degenerated into probe chains — measured 11.3% SLOWER than SipHash on
/// akiyo_cif. `mi_col` is the fastest-varying field, so it goes in the low bits.
///
/// The entries stay in a hash map rather than a flat table on purpose. A dense
/// `mi_rows*mi_cols*13` array is 27 MB at 1080p and the per-tile decision workers CLONE
/// the encoder, so a flat table would trade ~100 ns/op for tens of MB of allocation and
/// zeroing per frame per worker — a much worse deal than the hashing it removes.
///
/// `VP9_MODEMAP_STD` selects the old tuple/SipHash backend so the two can be alternated
/// inside one process; cross-process A/B on this codebase has produced flatly
/// contradictory numbers (see `poolbench`), so both arms live here together.
#[derive(Clone, Default)]
struct ModeMap {
    fast: std::collections::HashMap<u32, (ModeInfo, Mv, u8), BuildFxHasher>,
    std_: std::collections::HashMap<(usize, usize, usize), (ModeInfo, Mv, u8)>,
    use_std: bool,
}

#[inline(always)]
fn mm_key(mi_row: usize, mi_col: usize, bsize: usize) -> u32 {
    debug_assert!(mi_row < (1 << 13) && mi_col < (1 << 13) && bsize < (1 << 5));
    ((bsize as u32) << 26) | ((mi_row as u32) << 13) | mi_col as u32
}

impl ModeMap {
    fn new() -> ModeMap {
        let use_std = match MODEMAP_STD.load(std::sync::atomic::Ordering::Relaxed) {
            0 => false,
            1 => true,
            _ => {
                let v = std::env::var("VP9_MODEMAP_STD").is_ok();
                MODEMAP_STD.store(v as u8, std::sync::atomic::Ordering::Relaxed);
                v
            }
        };
        ModeMap { use_std, ..Default::default() }
    }
    #[inline]
    fn get(&self, mi_row: usize, mi_col: usize, bsize: usize) -> Option<(ModeInfo, Mv, u8)> {
        let _mm = prof::Scope::new(prof::S::ModeMap);
        if self.use_std {
            self.std_.get(&(mi_row, mi_col, bsize)).copied()
        } else {
            self.fast.get(&mm_key(mi_row, mi_col, bsize)).copied()
        }
    }
    #[inline]
    fn insert(&mut self, mi_row: usize, mi_col: usize, bsize: usize, v: (ModeInfo, Mv, u8)) {
        let _mm = prof::Scope::new(prof::S::ModeMap);
        if self.use_std {
            self.std_.insert((mi_row, mi_col, bsize), v);
        } else {
            self.fast.insert(mm_key(mi_row, mi_col, bsize), v);
        }
    }
    fn clear(&mut self) {
        self.fast.clear();
        self.std_.clear();
    }
    /// Merge a decision worker's map in. Tile columns are disjoint, so no key can
    /// collide and the merge order cannot matter.
    fn merge(&mut self, other: ModeMap) {
        self.fast.extend(other.fast);
        self.std_.extend(other.std_);
    }
}

static MODEMAP_STD: std::sync::atomic::AtomicU8 = std::sync::atomic::AtomicU8::new(2);

/// Select the old tuple/SipHash backend at runtime, so a bench can ALTERNATE the arms
/// inside one process (same reasoning as `set_snap_pool`).
pub fn set_modemap_std(on: bool) {
    MODEMAP_STD.store(on as u8, std::sync::atomic::Ordering::Relaxed);
}

/// FxHash — the rustc/Firefox multiply-rotate mixer, reproduced here rather than pulled
/// in as a dependency (it is four lines). For a single `u32` write this is one multiply
/// and one rotate against SipHash-1-3's full keyed permutation. It is NOT collision
/// resistant, which is irrelevant: the keys are our own dense coordinates, never
/// attacker-supplied.
#[derive(Clone, Default)]
struct BuildFxHasher;
impl std::hash::BuildHasher for BuildFxHasher {
    type Hasher = FxHasher;
    fn build_hasher(&self) -> FxHasher {
        FxHasher(0)
    }
}
struct FxHasher(u64);
impl std::hash::Hasher for FxHasher {
    #[inline]
    fn write(&mut self, bytes: &[u8]) {
        for &b in bytes {
            self.write_u8(b);
        }
    }
    #[inline]
    fn write_u8(&mut self, i: u8) {
        self.add(i as u64);
    }
    #[inline]
    fn write_u32(&mut self, i: u32) {
        self.add(i as u64);
    }
    #[inline]
    fn write_u64(&mut self, i: u64) {
        self.add(i);
    }
    #[inline]
    fn write_usize(&mut self, i: usize) {
        self.add(i as u64);
    }
    #[inline]
    fn finish(&self) -> u64 {
        self.0
    }
}
impl FxHasher {
    #[inline(always)]
    fn add(&mut self, i: u64) {
        const SEED: u64 = 0x51_7c_c1_b7_27_22_0a_95;
        self.0 = (self.0.rotate_left(5) ^ i).wrapping_mul(SEED);
    }
}

/// Observe-only reference-selection histogram (`VP9_REF_HIST=1`): [LAST, GOLDEN, ALTREF,
/// compound] counts over emitted inter blocks. Answers whether a coded ALT-REF is actually
/// being CHOSEN as a predictor — the difference between "the tool is useless on this
/// content" and "the tool is coded but never consulted".
pub static REF_HIST: [std::sync::atomic::AtomicU64; 4] =
    [const { std::sync::atomic::AtomicU64::new(0) }; 4];

fn ref_hist_on() -> bool {
    static E: std::sync::OnceLock<bool> = std::sync::OnceLock::new();
    *E.get_or_init(|| std::env::var("VP9_REF_HIST").is_ok())
}

/// Read and clear the reference histogram.
pub fn ref_hist_take() -> [u64; 4] {
    std::array::from_fn(|i| REF_HIST[i].swap(0, std::sync::atomic::Ordering::Relaxed))
}

/// Cap the free-lists: the partition recursion holds at most ~3 snapshots per level
/// over 4 levels, so a couple of dozen buffers covers every live snapshot with room
/// to spare. Unbounded lists would hoard the largest capacity ever seen, per thread.
const SNAP_POOL_CAP: usize = 48;

/// Oracle gate: `VP9_NO_SNAP_POOL=1` restores plain per-snapshot allocation, so the
/// pooled and unpooled arms can be A/B'd inside ONE binary (same thermal state, same
/// build) rather than across two.
fn snap_pool_enabled() -> bool {
    match SNAP_POOL.load(std::sync::atomic::Ordering::Relaxed) {
        0 => false,
        1 => true,
        _ => {
            let on = std::env::var("VP9_NO_SNAP_POOL").is_err();
            SNAP_POOL.store(on as u8, std::sync::atomic::Ordering::Relaxed);
            on
        }
    }
}
static SNAP_POOL: std::sync::atomic::AtomicU8 = std::sync::atomic::AtomicU8::new(2);

/// Flip the pool at runtime so a bench can ALTERNATE the two arms inside one process.
/// Comparing two separate runs cannot separate the change from machine state: the first
/// attempt here read -10.6% and the second +16.9%, with stages the pool never touches
/// moving just as much. Interleaving is the only honest A/B.
pub fn set_snap_pool(on: bool) {
    SNAP_POOL.store(on as u8, std::sync::atomic::Ordering::Relaxed);
    if !on {
        let _ = POOL_U16.try_with(|p| p.borrow_mut().clear());
        let _ = POOL_U8.try_with(|p| p.borrow_mut().clear());
        let _ = POOL_MI.try_with(|p| p.borrow_mut().clear());
    }
}

macro_rules! pool_ops {
    ($take:ident, $put:ident, $pool:ident, $t:ty) => {
        #[inline]
        fn $take(cap: usize) -> Vec<$t> {
            if !snap_pool_enabled() {
                return Vec::with_capacity(cap);
            }
            let mut v = $pool.try_with(|p| p.borrow_mut().pop()).ok().flatten().unwrap_or_default();
            v.reserve(cap);
            v
        }
        #[inline]
        fn $put(mut v: Vec<$t>) {
            if v.capacity() == 0 || !snap_pool_enabled() {
                return;
            }
            v.clear();
            // `try_with`: a snapshot can outlive TLS destruction at thread teardown, in
            // which case the buffer simply frees normally.
            let _ = $pool.try_with(|p| {
                let mut p = p.borrow_mut();
                if p.len() < SNAP_POOL_CAP {
                    p.push(v);
                }
            });
        }
    };
}
pool_ops!(take_u16, put_u16, POOL_U16, u16);
pool_ops!(take_u8, put_u8, POOL_U8, u8);
pool_ops!(take_mi, put_mi, POOL_MI, ModeInfo);

impl Drop for BlockSnap {
    fn drop(&mut self) {
        let _sd = prof::Scope::new(prof::S::SnapDrop);
        for v in &mut self.rec {
            put_u16(std::mem::take(v));
        }
        for v in &mut self.above_ctx {
            put_u8(std::mem::take(v));
        }
        put_u8(std::mem::take(&mut self.above_seg));
        put_mi(std::mem::take(&mut self.mi));
    }
}

/// One reconstructed/source plane (coded size: `mi_*·8 >> ss`).
#[derive(Clone)]
struct Plane {
    buf: Vec<u16>,
    stride: usize,
    ss_x: usize,
    ss_y: usize,
    w: usize,
    h: usize,
}

/// Intra key-frame encoder. Coordinates are in the coded grid (rounded up to 8).
#[derive(Clone)]
pub struct FrameEncoder {
    width: u32,
    height: u32,
    mi_rows: usize,
    mi_cols: usize,
    qindex: u32,
    src: [Plane; 3],
    /// u8 mirror of the luma source (8-bit content only) — the search domain:
    /// half the load traffic of u16 and `psadbw` SADs, bit-identical values.
    src8: Vec<u8>,
    /// Lazily-built u8 mirrors of the reference lumas, one per ref slot.
    refs8: std::cell::RefCell<[Option<std::sync::Arc<[u8]>>; 3]>,
    rec: [Plane; 3],
    mi: Vec<ModeInfo>,
    above_seg: Vec<u8>,
    left_seg: [u8; 8],
    above_ctx: [Vec<u8>; 3],
    left_ctx: [[u8; 16]; 3],
    dq_y: (i32, i32),
    dq_uv: (i32, i32),
    /// Activity-based ADAPTIVE QUANTIZATION (AQ, `VP9_AQ=<delta>`): per-SB variance sorts
    /// each 64×64 into a low/high-activity SEGMENT that carries a per-segment ALT_Q qindex
    /// delta (VP9 segmentation). `aq` = the delta magnitude (0 = off; sign = direction).
    /// `aq_seg` holds the per-SB segment id; `aq_dq_y/uv`/`aq_lambda` the resolved per-segment
    /// dequant + RD-λ (set into `dq_y`/`dq_uv`/`lambda` at each SB root). `aq_ncols` = SB cols.
    aq: i32,
    /// This frame actually uses AQ: `aq != 0` AND the content gate passed (frame median SB
    /// variance ≤ `VP9_AQ_MAXVAR`). AQ is a SSIM win on low-activity content but a loss on
    /// uniformly high-activity content (mobile), so it's content-adaptively enabled per frame.
    aq_active: bool,
    aq_seg: Vec<u8>,
    aq_ncols: usize,
    aq_dq_y: [(i32, i32); 2],
    aq_dq_uv: [(i32, i32); 2],
    aq_lambda: [f64; 2],
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
    /// The CURRENT block's motion-compensation filter (0=EIGHTTAP/1=SMOOTH/2=SHARP).
    /// Distinct from the frame-header `interp_filter` (which is 4=SWITCHABLE when the
    /// per-block filter is coded): the MC/pred_sse read THIS, set per block by the
    /// filter RD search (switchable) or held at the fixed frame filter otherwise.
    active_filter: u8,
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
    /// RD-trial early-abort bound: when a luma trial's running `sse + λ·bits`
    /// strictly exceeds this, `encode_plane` returns the (u64::MAX, u64::MAX)
    /// sentinel — the candidate has provably lost to the incumbent.
    trial_abort_at: Option<f64>,
    /// When set, the trial (`encode_plane(None)`) applies the trellis just like the
    /// commit — so the skip decision sees the *post-trellis* EOB (the trellis can
    /// empty a block the raw quantizer didn't).
    skip_trial: bool,
    // R5 — AC deadzone: the AC rounding offset as `ac_step·ac_round_num/8`.
    // 4 = round-to-nearest; 3 rounds AC toward zero (RD-aware deadzone).
    ac_round_num: i64,
    // R5 — trellis-style RD-optimal end-of-block (drop trailing coefficients by RD).
    use_trellis: bool,
    /// Trellis-λ scale (`VP9_TRELLIS_LAMBDA`, default 1.0): the RDOQ uses `self.lambda ·
    /// this`. >1 zeros/trims MORE aggressively (fewer coeff bits, more distortion), <1 less.
    trellis_lambda_scale: f64,
    /// Content-adaptive trellis-λ strength (`VP9_TRELLIS_K`): the RDOQ λ is scaled by
    /// `1 + k·(eob/n)` — DENSE blocks (many coeffs = noisy high-motion residual) trim
    /// aggressively, SPARSE blocks (few coeffs = static detail) keep λ≈self.lambda. A
    /// sign-flip lever: a flat high λ wins high-motion but loses static; this dispatches it.
    trellis_k: f64,
    // Trellis distortion: false = fast coefficient-domain estimate (parity with
    // libvpx, default); `VP9_TRELLIS_EXACT=1` = the exact pixel-SSE oracle.
    trellis_exact: bool,
    // Roof — per-block transform-size search (4×4 vs 8×8 for 8×8 luma blocks).
    use_tx_search: bool,
    /// Content-adaptive tx-size search ORDER (`VP9_NO_TXORDER` disables): likely-best-first
    /// so the abort prunes losers. Byte-identical; a speed lever only.
    tx_order: bool,
    /// tx-size search early-break factor (`VP9_TX_THRESH`, 0 = off, default): once a
    /// later size's J exceeds the confident best by this factor, stop the search.
    /// MEASURED NEGATIVE (2026-07-23): a content SIGN-FLIP, not a clean win — @1.15
    /// akiyo/mobile gain (BD −0.47/−0.06%, +5/+2%) but foreman/bus REGRESS (BD +0.54/
    /// +0.13%, and slower). The tx-size RD is already well-calibrated (tx_order + abort
    /// + tx-cost charge); the marginal speed doesn't justify a per-content dispatch.
    /// Kept env-gated as the documented A/B; default off ⇒ full search unchanged.
    tx_thresh: f64,
    // When the full tx-size search is OFF (fast presets), the mode search + final
    // coding start from the block's MAX tx instead of 4×4. Falling back to 4×4 was
    // both a transform flood (RD trials transform the whole block at 4×4 for every
    // candidate) AND poor compression (large tx codes smooth residual far cheaper).
    // `VP9_TX4X4=1` restores the historical 4×4 default (the A/B oracle).
    tx4x4: bool,
    // Max tx size the fast-preset default uses (0=4×4 … 3=32×32). 1 (8×8) is the
    // speed-neutral sweet spot; `VP9_TXCAP` overrides for A/B (2/3 improve
    // compression but our naive large-tx kernels make them slower).
    tx_cap: u8,
    // Brick 2 — skip the 4-mode intra alternative on an inter block when the best
    // inter mode is already good (`best_inter.J / λ < intra_gate_t`). Intra almost
    // never wins on a well-predicted inter block, so this drops those 4 full-RD
    // transforms. `VP9_NO_INTRA_GATE` restores the always-try oracle;
    // `VP9_INTRA_GATE_T` tunes the threshold (higher ⇒ try intra more ⇒ safer).
    intra_gate: bool,
    intra_gate_t: f64,
    // Brick 2b — inter-mode SHORTLIST: rank all (ref×mode) candidates by the cheap
    // skip-RD estimate J_skip = pred_SSE + λ·bits and full-RD only the top
    // `shortlist_k`, instead of a full transform trial on every candidate.
    // `VP9_NO_SHORTLIST` full-RDs all; `VP9_SHORTLIST_K` tunes K.
    mode_shortlist: bool,
    shortlist_k: usize,
    // Roof — partition control. `force_min_bsize` codes PARTITION_NONE once a block
    // reaches this size (default BLOCK_8X8 = the historical all-8×8). Larger values
    // bring up bigger blocks; `use_partition_rd` turns on the recursive RD search.
    force_min_bsize: usize,
    // Brick 2: offer sub-8×8 (4×4) inter prediction at BLOCK_8X8 (env `VP9_SUB8X8`).
    sub8x8: bool,
    use_partition_rd: bool,
    /// Partition decision recorded by the RD pass, keyed by `(mi_row, mi_col,
    /// bsize)`; read by `encode_partition` during the emit pass(es).
    part_map: std::collections::HashMap<(usize, usize, usize), u8>,
    /// Leaf mode decisions recorded by the RD pass (same key as `part_map`),
    /// reused by the emit pass(es) instead of re-running the whole mode search.
    /// Exact: a decision is a deterministic function of the block's entry state,
    /// and the winning path's entries were computed under the emit-identical
    /// z-order state (candidates snapshot/restore around themselves). The third
    /// element is the tx size the skip trial ran at (`last_trial_tx`) — a cached
    /// SKIP block must replay that trial to leave the MC recon + zeroed entropy
    /// contexts in place, exactly as `decide_*` did (the commit emits no tokens
    /// for skip blocks and relies on those side effects).
    mode_map: ModeMap,
    /// Tx size the most recent `decide_*` trial reconstructed with (see `mode_map`).
    last_trial_tx: u8,
    /// Prediction-only SSE (`Σ(src−pred)²`) accumulated during the skip trial — the
    /// distortion the block would have if coded `skip` (recon == MC prediction). Feeds
    /// the RD skip decision (`rd_skip`).
    pending_pred_sse: u64,
    /// When set, the trial forces every tx block's EOB to 0 — reproducing an empty
    /// (skip) block's recon (MC prediction, no residual) + zeroed entropy contexts.
    /// Used to re-materialise a block the RD skip decision chose to drop.
    force_skip: bool,
    /// RD skip decision (libvpx `x->skip`): even when the residual quantises to a few
    /// coefficients, drop the whole residual when `J_skip = pred_sse + λ·rate(skip=1)`
    /// beats `J_noskip = recon_sse + λ·(coef_bits + rate(skip=0))`. Default ON (corpus
    /// BD-rate −2.75% CIF / −0.57% 1080p, bit-exact); `VP9_NO_RD_SKIP` disables it.
    rd_skip: bool,
    /// Partition-cascade lever (`VP9_SPLIT_PEN`, DEFAULT 1.0 = OFF): when the NONE arm
    /// codes `skip`, multiply the SPLIT/sub RD by this before the NONE-vs-SPLIT compare,
    /// biasing static regions toward one large skip block (fewer per-block headers).
    /// Measured 2026-07-10: CONTENT-DEPENDENT, not a clean default. At 1.02: CIF −0.55%
    /// BD (akiyo −1.60%, the static win) but 1080p +1.44% (park_joy +2.76% — busy texture
    /// needs fine partitions even when skipping). Bit-exact/conformant. Kept opt-in for
    /// static/low-res content; a frame-motion gate could make it a keeper (see memory).
    split_penalty: f64,
    /// AVX2 available (cached once; the SAD hot loop dispatches on it per call).
    has_avx2: bool,
    /// Exhaustive ±8 integer motion search (speed-0 default). The diamond search
    /// (`false`) measured +2.05% BD-rate for only ~1.2× speed once the AVX2 SAD
    /// landed, so it's a speed-preset lever (speed ≥ 1), not the quality default.
    /// `VP9_DIAMOND_MSEARCH` forces diamond; `VP9_FULL_MSEARCH` forces exhaustive.
    full_msearch: bool,
    /// x4-batched exhaustive integer search (`VP9_NO_MSEARCH_X4` disables → the
    /// one-position-at-a-time scalar oracle). The x4 kernel scores four ref positions
    /// from one source-tile load; byte-identical to four scalar SADs (A/B oracle).
    msearch_x4: bool,
    /// `VP9_CORNER_SAD`: restore the corner-only (top-left 8×8) integer-search
    /// scoring — the oracle the full-block SAD was BD-rate-gated against.
    corner_sad: bool,
    /// Sub-8×8 NEWMV pre-screen threshold (SAD): skip the per-4×4 NEWMV search
    /// when a free predicted mode already fits at least this well. 0 = always
    /// search (the speed-0 default); set by `VP9_SUB8X8_PRESCREEN` or presets.
    sub8x8_prescreen: i64,
    /// `VP9_SUB8_PROBE`: observe-only sub-8×8 compound ceiling harvest (SAD reduction).
    sub8_probe: bool,
    /// `VP9_LFSEG_PROBE`: observe-only per-segment loop-filter ceiling harvest (SSE reduction).
    lfseg_probe_on: bool,
    /// Multi-reference sub-8×8 (`VP9_NO_SUB8X8_MULTIREF` opts out): sub-8×8 is otherwise
    /// hardcoded to LAST — try GOLDEN/ALTREF too and pick the best single ref per 8×8
    /// (no added MV bits, just a better reference for fine-motion leaves).
    sub8x8_multiref: bool,
    /// SAD penalty (`VP9_SUB8X8_REF_PENALTY`) added to a non-LAST sub-8×8 ref's cost so the
    /// SAD proxy only switches away from LAST on a real margin — the proxy under-prices
    /// non-LAST (equal-residual static content over-selected GOLDEN/ALTREF for ref bits).
    sub8x8_ref_penalty: f64,
    /// Content gate (`VP9_SUB8X8_MULTIREF_GATE`, SAD): only pay the GOLDEN/ALTREF sub-8×8
    /// search when LAST's summed sub-block SAD exceeds this — LAST-fits-well leaves stay
    /// LAST-only (byte-identical, no cost), so the ~3× search cost lands only on hard leaves.
    sub8x8_multiref_gate: f64,
    /// Iterative (diamond) subpel refinement instead of the 5×5 grid — a preset
    /// lever (speed ≥ 1); the quality default keeps the exhaustive grid.
    subpel_fast: bool,
    /// Subsample the full-block interp scoring in the mode-shortlist `pred_sse`
    /// (2× tile stride, SSE scaled back) — a speed lever; the shortlist only ranks
    /// candidates, so the coarser estimate costs little quality.
    motion_fast: bool,
    /// Score subpel refinement with the 2-tap BILINEAR filter (libvpx
    /// `sub_pixel_tree` semantics) instead of the commit-grade 8-tap — the
    /// ranking approximation that makes their search ~2× cheaper. BD-gated.
    subpel_bilinear: bool,
    /// Skip the ¼-pel ring when the ½-pel ring found no improvement.
    subpel_tree: bool,
    /// Plus+diagonal single-pass subpel shape (libvpx tree geometry).
    subpel_diag: bool,
    /// High-precision (⅛-pel) motion vectors (`allow_high_precision_mv`). DEFAULT ON:
    /// the subpel search refines to ⅛-pel and MVs code at ⅛-pel — but ONLY where
    /// `use_mv_hp(predictor)` holds (small MVs, <8 px), which is both the codability
    /// condition AND an inherent content gate, so only low-motion blocks pay the extra
    /// subpel level (akiyo ~1.15×, high-motion bus/mobile ~1.06×). A −3.13% BD-rate win
    /// @s3 (mobile −7.18%, all clips win) for that tiny cost. Bit-exact vs libvpx.
    /// `VP9_NO_HP_MV` escapes to ¼-pel (the old behaviour).
    hp_mv: bool,
    /// COMPOUND (bi-directional) prediction. When on (inter frame, GOLDEN present), the
    /// frame codes `reference_mode = SELECT` with GOLDEN as the fixed compound ref
    /// (`sign_bias[GOLDEN]=1` ⇒ comp_fixed_ref=2, comp_var_ref=[LAST,ALTREF]); blocks may
    /// pick LAST+GOLDEN compound (averaged prediction `(p0+p1+1)>>1`). `VP9_COMPOUND`
    /// enables; `VP9_COMPOUND_FORCE` forces every inter block to ZEROMV-compound (a
    /// conformance-plumbing probe). Default OFF — baseline byte-identical.
    compound: bool,
    /// Force every inter block to ZEROMV-compound (Brick-1 plumbing gate).
    compound_force: bool,
    /// Opt out of the default-on bi-prediction (`VP9_NO_COMPOUND`).
    no_compound: bool,
    /// The ALTREF slot holds a FUTURE frame (set by `set_altref`, i.e. an ARF-group P
    /// frame referencing the group's hidden last frame). Enables TRUE bi-prediction:
    /// `sign_bias[ALTREF]=1` is the CORRECT display-order bias (no MV-pred corruption,
    /// unlike the LAST+GOLDEN sign-bias trick), and ALTREF becomes the fixed compound ref.
    altref_future: bool,
    /// Content-adaptive compound gate: only spend the compound RD trials on blocks whose
    /// single-ref winner J exceeds `compound_gate · λ` — i.e. blocks the single ref
    /// predicts POORLY, where bi-prediction can help (search-skip gate; λ-normalized so
    /// the threshold transfers across QPs). 0 = always try. `VP9_COMPOUND_GATE` sets it.
    compound_gate: f64,
    /// Add NEAREST/NEAR compound modes (derived MVs, no bits). Conformant (decoder-exact
    /// `find_mv_refs(ref,mode)[idx]`), but DEFAULT-OFF: a net BD LOSS — mobile regresses
    /// (the RD under-prices the derived-MV modes → over-selection). `VP9_COMPOUND_NEAR` on.
    compound_near: bool,
    /// Compound-aware switchable interp-filter search (`VP9_NO_COMPOUND_FILTER` opts out):
    /// score the filter on the actual AVERAGED compound prediction at the two compound MVs,
    /// not the single-ref `pred_sse(best_mv)` (which scores the wrong ref at the wrong MV).
    compound_filter: bool,
    /// JOINT compound MV refinement (`VP9_NO_COMPOUND_JOINT` opts out): refine each
    /// NEWMV-compound MV against the averaged prediction and add the refined pair as an EXTRA
    /// candidate for the full RD to pick — a clean BD win on all clips (~−0.05..−0.25%, 0
    /// desyncs). The earlier REPLACE form lost (the SAD-8×8 proxy over-moves); making it a
    /// proposal the full-block RD can reject (propose-cheap/dispose-by-RD) turned it positive.
    compound_joint: bool,
    /// Cost penalty (bits) added to NEAREST/NEAR compound candidates to correct the RD
    /// under-pricing that over-selects them (they win on averaged SSE but spend bits for
    /// ~0 quality). `VP9_COMPOUND_NEAR_PEN`.
    compound_near_penalty: f64,
    /// Speed >= 3: abort SPLIT recursion once its running cost exceeds NONE.
    split_early: bool,
    /// Speed >= 3: stop shortlist trials once J_skip > best_J × this (0 = off).
    mode_thresh_mult: f64,
    /// Predict the loop-filter level from q instead of searching (`VP9_LF_FROM_Q`).
    lf_from_q: bool,
    /// Speed >= 3: 64×64 G1 partition-gate threshold in none_rd/λ units (0 = off).
    g1_64: f64,
    /// DP-lite: frozen-context pricing for interior magnitude lowerings
    /// (libvpx optimize_b's approximation). `VP9_TRELLIS_EXACT_CTX` disables.
    trellis_frozen: bool,
    /// libvpx-mirrored residual-MSE trellis gate threshold (0 = gate off).
    trellis_mse_t: f64,
    /// Emit-dedup (`VP9_DEDUP` opt-in, DEFAULT OFF): cache each skip-trial's
    /// (levels, dqcoeff, eob) keyed by residual hash so the emit pass can reuse
    /// them instead of re-running fwd+quantize+trellis. Reuse is byte-identical
    /// (pure function of the residual). ★ MEASURED NET-NEGATIVE (2026-07-23):
    /// the store runs in the DECISION pass (95% of encode) — an FNV hash over
    /// the residual + a HashMap insert with TWO `to_vec()` heap allocs, PER tx
    /// block PER trial (most for losing arms the emit never reuses) — to save the
    /// EMIT pass (only ~4%). Disabling it is +2.2% (mobile s0) … +9.6% (akiyo s3),
    /// byte-identical on every clip×speed. Paying the expensive pass to save the
    /// cheap one; kept opt-in only as the A/B oracle.
    emit_dedup: bool,
    #[allow(clippy::type_complexity)]
    dedup_map: std::cell::RefCell<
        std::collections::HashMap<(u8, u32, u32, u8), (u64, Vec<i32>, Vec<i32>, u16)>,
    >,
    /// `VP9_TRIAL_RECON=1`: restore exact pixel recon+SSE in inter mode-trials
    /// (the A/B oracle for the Parseval trial-distortion estimate).
    trial_recon: bool,
    /// Run the trellis inside exploration skip-trials. DEFAULT ON — measured
    /// LOAD-BEARING for the RD-skip decision (fast trials inflate j_noskip via
    /// non-trellised coef bits → systematic over-skip, +21.5% BD mean, mobile
    /// +50%). `VP9_FAST_TRIALS=1` opts into the fast mode for future study.
    trellis_trials: bool,
    /// Max re-centering rounds per subpel precision level (0 = unbounded, the
    /// original shape). One-round measured +5% BD (REFUTED); the cap trades
    /// tail scores for bounded quality cost.
    subpel_rounds: u32,
    /// `VP9_G1_HARVEST`: observe-only partition-gate telemetry (G1) to stderr.
    g1_harvest: bool,
    /// Current tile's mi-column range (whole frame when single-tile). Entropy
    /// contexts, MV refs, and left-availability are bounded by these, exactly
    /// mirroring the decoder's `tile_col_start/end` (`tile_offset`).
    tile_start: usize,
    tile_end: usize,
    /// log2 of tile columns (header `tile_cols_log2`); auto ≥16 SB cols → 2.
    tile_cols_log2: u32,
    /// F3 context chaining: code this frame against the (companion-decoder
    /// adapted) previous context instead of defaults, with temporal MV
    /// prediction — `error_resilient=0`, mirroring the decoder's rules.
    chain: bool,
    /// Previous frame's per-mi motion records (temporal MV candidates).
    prev_mvs: Option<std::sync::Arc<Vec<MvRef>>>,
    /// G1 partition gate (skip SPLIT/sub-8x8 when NONE's RD is below the discovered
    /// per-bsize threshold). `VP9_NO_G1GATE` disables (the exhaustive oracle).
    g1_gate: bool,
    /// G3 ref shortlist (skip GOLDEN/ALTREF when LAST's J is below threshold).
    g3_gate: bool,
    /// G1 threshold scale — 1.0 at speed 0; presets raise it (a speed/BD trade
    /// the G2 sweep mapped: bumps help motion clips, cost static ones).
    g1_scale: f64,
    /// Resolution-aware multiplier on the G1 partition gate, `(pixels/CIF)^alpha`.
    /// `VP9_G1_AREA` sets alpha; **default 0.0 = off, and REFUTED — see below.**
    ///
    /// The motivating measurement is sound: on identical content at two resolutions
    /// (city, 352x288 vs 704x576, 30 frames, both encoders at their quality preset)
    /// 4x the pixels costs libvpx 1.71x the time and costs us 2.64x, so libvpx
    /// amortizes 2.34x per pixel against our 1.51x — a real 1.55x scaling deficit.
    /// The theory was that larger frames spread detail over more pixels, so a big
    /// partition is more often optimal and the gate can afford to be aggressive.
    ///
    /// BD-rate REFUTED it. 4 CRFs x 30 frames, PSNR and rate over the same frames:
    ///
    /// ```text
    ///            alpha=0.3            alpha=0.6
    ///   city     +4.25% BD / -11.8%   +6.59% BD / -22.7%
    ///   harbour  +1.17% BD / -15.7%   +1.86% BD / -25.0%
    ///   soccer   +2.81% BD / -12.8%   +8.50% BD / -25.2%
    /// ```
    ///
    /// Worse on 6 of 6 cells. This reproduces the existing `set_speed` finding that
    /// escalating `g1_scale` is the toxic lever because the partition gate prunes
    /// exactly where split matters — making that escalation resolution-aware does
    /// not rescue it. The scaling deficit is real but the partition gate is the
    /// WRONG lever for it; the knob is kept default-off so the next attempt starts
    /// from the measurement rather than repeating it.
    g1_area: f64,
    /// Raw SSE of the most recent NONE trial (`rd_block_none`). Kept for the G1 gate
    /// diagnosis recorded on `g1_64`; no live consumer.
    last_none_sse: u64,
    /// Content-adaptive partition: route this superblock's partition decision
    /// through the O(pixels) variance tree (`varpart`) instead of the recursive RD
    /// search. `VP9_VAR_PART=1` forces it ON for EVERY SB (the Brick-2 standalone
    /// A/B — a whole-frame variance partition); the dispatcher (Brick 3) sets it
    /// per-SB. Content-invariant cost: the lever that flattens the ~15× content
    /// speed variance.
    var_part: bool,
    /// Variance split threshold multiplier: an SB node splits when its residual
    /// variance ≥ `var_thresh_mult · ac_dequant · level_scale`. Higher ⇒ coarser
    /// partitions ⇒ faster/lower-quality. `VP9_VAR_THRESH` overrides for sweeps.
    var_thresh_mult: f64,
    /// The current superblock's variance tree (built at the 64×64 root, read by the
    /// recursion). Transient per-SB scratch; `None` outside the variance path.
    vt: Option<VarTree>,
    /// Content-adaptive DISPATCH: per-SB, choose the variance partition when the SB's
    /// root residual variance is below `dispatch_thresh` (RD wins nothing there — it
    /// over-splits flat blocks), else the full RD search (where RD's finer partitions
    /// pay). This is the "one encoder" core: RD quality where it matters, variance
    /// speed where it doesn't. `VP9_DISPATCH=1` enables; Brick 4 adapts the threshold
    /// per frame. Independent of `var_part` (which forces variance for every SB).
    dispatch: bool,
    /// Root-variance cutoff for the dispatcher (SB uses variance partition when its
    /// 64×64 residual variance is on the variance side of this). `VP9_DISPATCH_T`
    /// pins it (fixed-T mode); otherwise Brick 4's per-frame pre-pass sets it from
    /// the actual variance distribution (`dispatch_q`).
    dispatch_thresh: i64,
    /// `true` ⇒ pin `dispatch_thresh` (skip the per-frame percentile pre-pass) —
    /// set when `VP9_DISPATCH_T` is given, for threshold sweeps.
    dispatch_fixed_t: bool,
    /// Target FRACTION of superblocks routed to the variance partition (Brick 4).
    /// The per-frame pre-pass sets `dispatch_thresh` to the matching percentile of
    /// this frame's SB root variances, so the routing fraction — hence the relative
    /// work — is content-invariant. `VP9_DISPATCH_Q` overrides (default 0.5).
    dispatch_q: f64,
    /// Dispatch DIRECTION: `false` (default) routes the LOW-variance SBs to the
    /// variance partition (quality-optimal — RD kept for the busy SBs where it
    /// helps); `true` routes the HIGH-variance (most RD-expensive) SBs to variance
    /// (time-capping — the small quality edge RD holds on busy content is cheap to
    /// concede). `VP9_DISPATCH_HI=1` selects the time-capping direction.
    dispatch_hi: bool,
    /// Lever 2 — the decision-pass wall time (µs) this frame spent, measured in
    /// `encode_frame` and read back by the outer encoder's time-budget controller
    /// (which owns the cross-frame `dispatch_q` state, since a `FrameEncoder` is
    /// per-frame). 0 until the frame is encoded.
    decision_us: u64,
    /// Model-based early SKIP (libvpx non-RD philosophy, CALIBRATED): force skip with
    /// an MC-only recon — avoiding the decision pass's per-block transform — when the
    /// block's `varrd::model_xsq` (normalized quantizer²/residual-variance) is above
    /// `model_skip_t`. A non-skip falls through to the normal transform (the real
    /// eob-based decision), so no decision/emit desync. The threshold is CALIBRATED
    /// from a harvest (log2(xsq) vs the real rd_skip): skip% rises with xsq and plateaus
    /// ~94% (variance misses localized detail), so a conservative cutoff skips only
    /// high-confidence blocks. `VP9_MODEL_SKIP=1` enables; `VP9_MODEL_SKIP_T` sets the
    /// log2(xsq) cutoff (default 23 ≈ 91% real-skip). DEFAULT OFF.
    model_skip: bool,
    /// log2(xsq) cutoff for `model_skip` (higher ⇒ fewer, safer skips). See above.
    model_skip_t: u32,
    /// NON-RD LEAF mode (floor-lowering): when a superblock is routed to the variance
    /// partition (the dispatcher's fast arm), its leaves take a CHEAPER `decide_inter`
    /// — LAST-ref only (no GOLDEN/ALTREF motion search + candidate transforms) and a
    /// forced model-SKIP gate (MC-only recon on small-residual leaves, no transform).
    /// This lowers the all-variance floor so the time-budget controller can reach
    /// faster per-frame targets on complex content. Confined to variance-routed SBs
    /// (`variance_leaf`), so the RD partition path is untouched. `VP9_NONRD_LEAF=1`.
    nonrd_leaf: bool,
    /// Runtime flag: currently inside a variance-partition leaf (`var_pick_partition`
    /// sets it around `rd_block_none`), so `decide_inter` takes the `nonrd_leaf` fast
    /// path. False on the RD partition path — the fast leaf never touches full RD.
    variance_leaf: bool,
    /// Non-RD leaf NEARESTMV early-out (search-skip gate): skip the NEWMV diamond +
    /// subpel search when the predictor's SAD-per-pixel is below this — the block is
    /// already well-predicted, so NEWMV≈NEARESTMV and the search is redundant (unlike
    /// cutting subpel PRECISION, this skips work that wouldn't change the answer).
    /// 0 = off. Decision-only (MV recorded + replayed). `VP9_NONRD_ME_SKIP` sets it.
    nonrd_me_skip: f64,
    /// CHROMA-aware inter mode RD: cost the full-RD candidates on luma+chroma
    /// (`rd_cost_yuv`) instead of luma alone (`rd_cost_y`). The mode/MV is luma-searched
    /// and chroma follows (MV/2), but the luma-best candidate isn't always the
    /// luma+chroma-best — colour structure the luma predictor misses (chroma edges) can
    /// pick a different mode. Adds NEW information the luma-only pick lacks (unlike the
    /// neutral model-RD reweighting). `VP9_CHROMA_RD=1`; gated on BD-rate.
    chroma_rd: bool,
    /// Weight on the chroma SSE + bits in `rd_cost_yuv` (`VP9_CHROMA_RD_W`, default 1.0).
    chroma_rd_w: f64,
    /// Luma-abort in `rd_cost_yuv` (`VP9_NO_YUV_ABORT` disables): skip the chroma trial
    /// reconstruct once the luma-only J already loses. Byte-identical; A/B toggle.
    yuv_abort: bool,
    /// Model-RD shortlist ranking (`VP9_MODEL_RANK`, default OFF): rank by the Laplacian
    /// model's estimated coded (dist, residual-rate) instead of raw pred_SSE. Tested: BD-neutral
    /// but does NOT let K shrink (K=2 still loses ~1% — our exact reconstructs are load-bearing),
    /// and the per-candidate model_rd cost makes it a MIXED speed wash (foreman +5%, mobile −3%).
    /// Kept for experiments.
    model_rank: bool,
    /// Snapshot the RECON in `snap_block` (default ON; `VP9_NO_SNAP_RECON` skips). The recon
    /// save/restore LOOKS redundant (trials overwrite, winner re-reconstructs) but SKIPPING IT
    /// IS NOT BYTE-IDENTICAL (measured — all clips diverge), so some path reads the pre-trial
    /// recon (a real dependency). Kept ON; skip only for experiments (would need BD-gating).
    snap_recon: bool,
    /// Reused per-tx-block scratch (see [`TxScratch`]). Boxed and moved out with
    /// `mem::take` for the duration of `encode_tx_block` so it does not conflict
    /// with the `&self.src` / `&mut self.rec` borrows that function needs.
    tx_scratch: Box<TxScratch>,
    /// Re-zero [`TxScratch`] on entry, reproducing the old stack-array cost
    /// exactly (`VP9_TX_MEMSET=1`). The A/B oracle for the scratch brick: output
    /// is byte-identical either way, so any difference is purely the memset.
    tx_memset: bool,
}

/// The five per-tx-block working buffers.
///
/// These used to be `[0i32; 1024]` locals declared inside `encode_tx_block`.
/// Every one is sized for the largest transform (32x32) but only `[..bs*bs]` is
/// ever touched, and a 4x4 luma block uses 16 of the 1024 entries — so each call
/// paid a ~17 KB zero-init to use as little as 320 bytes of it. `encode_tx_block`
/// runs 2.1M times on a 20-frame CIF clip at crf 32, which made that memset the
/// single largest unattributed bucket in the encoder.
///
/// Reuse is sound because every buffer is fully written over `[..n]` before it is
/// read over `[..n]`: `residual` by the differencing loop, `coeffs` by
/// `forward_transform`, `levels`/`dqcoeff` by `quantize` (or by the dedup cache's
/// `copy_from_slice`), and `token_cache` by the coefficient walk that consumes it.
/// Nothing carries meaning across calls, so stale bytes past `n` are unreachable.
#[derive(Clone)]
pub(crate) struct TxScratch {
    residual: [i32; 1024],
    coeffs: [i32; 1024],
    levels: [i32; 1024],
    dqcoeff: [i32; 1024],
    token_cache: [u8; 1024],
}

impl Default for TxScratch {
    fn default() -> TxScratch {
        TxScratch {
            residual: [0; 1024],
            coeffs: [0; 1024],
            levels: [0; 1024],
            dqcoeff: [0; 1024],
            token_cache: [0; 1024],
        }
    }
}

impl TxScratch {
    /// Reproduce the old per-call zero-init — the A/B arm, never the default.
    #[inline]
    fn clear(&mut self) {
        self.residual = [0; 1024];
        self.coeffs = [0; 1024];
        self.levels = [0; 1024];
        self.dqcoeff = [0; 1024];
        self.token_cache = [0; 1024];
    }
}

/// Read an `f64` tuning knob from the environment.
///
/// `set_speed` runs AFTER `new()`, so a preset assigning a knob directly would
/// silently overwrite whatever the environment asked for — which is exactly what
/// happened to `VP9_MODE_THRESH`, `VP9_G1_64`, `VP9_INTRA_GATE_T` and
/// `VP9_SUBPEL_TREE`: all four were read in `new()` and then clobbered by the
/// speed-3 block, so every sweep of them measured an unchanged encoder (verified:
/// four different `VP9_MODE_THRESH` values produced one identical bitstream).
/// Presets must therefore use `env_f64(..).unwrap_or(default)`, never a bare
/// assignment, or the knob is not sweepable at that tier.
fn env_f64(key: &str) -> Option<f64> {
    std::env::var(key).ok().and_then(|v| v.parse().ok())
}

impl FrameEncoder {
    /// Content-**activity**-adaptive λ multiplier for `λ = ac²·mult`, tuned for perceptual
    /// (VMAF) quality. Our RD minimises SSE, so the SSE-optimal λ under-codes the detail
    /// VMAF rewards; lowering λ preserves it — but only where there IS detail to preserve.
    ///
    /// Measured 2026-07-10: the right λ tracks CONTENT ACTIVITY, not resolution (the earlier
    /// resolution model was a proxy — detailed content wants low λ at 480p/720p/1080p alike,
    /// crowd_run BD-VMAF −7.7/−9.4/−10.8%). `activity` = mean |source − reference| luma.
    /// High activity (detail/motion) → low λ (0.0005): preserve detail, big VMAF win. Static
    /// content (akiyo, activity≈2) → high λ (0.0013): skip more, a PSNR win at ~0 VMAF cost
    /// (no detail to lose). Log-linear interp, calibrated to per-clip optima (akiyo 2.0→0.0013,
    /// foreman 5.9→~0.0008, crowd/mobile/bus/park 11–22→0.0005). Key/intra frames have no
    /// temporal signal → resolution fallback. `VP9_LAMBDA_MULT` fixes it; `VP9_LAMBDA_RES`
    /// forces the old resolution model (A/B).
    fn lambda_mult(activity: f64, is_inter: bool, width: u32, height: u32) -> f64 {
        if let Some(m) = std::env::var("VP9_LAMBDA_MULT")
            .ok()
            .and_then(|s| s.parse::<f64>().ok())
        {
            return m;
        }
        // Resolution model (old default / key-frame fallback): 0.0007 CIF → 0.0005 1080p.
        let res_mult = {
            const LO_PX: f64 = 101_376.0;
            const HI_PX: f64 = 2_073_600.0;
            let px = ((width as f64) * (height as f64)).max(1.0);
            let t = ((px.ln() - LO_PX.ln()) / (HI_PX.ln() - LO_PX.ln())).clamp(0.0, 1.0);
            0.0007 + t * (0.0005 - 0.0007)
        };
        if !is_inter || std::env::var("VP9_LAMBDA_RES").is_ok() {
            return res_mult;
        }
        const A_LO: f64 = 2.0; // ≤ this (static) → MULT_HI
        const A_HI: f64 = 11.0; // ≥ this (detail/motion) → MULT_LO
        const MULT_HI: f64 = 0.0013;
        const MULT_LO: f64 = 0.0005;
        let t = ((activity.max(0.1).ln() - A_LO.ln()) / (A_HI.ln() - A_LO.ln())).clamp(0.0, 1.0);
        let act_mult = MULT_HI + t * (MULT_LO - MULT_HI);
        // Resolution FLOOR: the same activity carries less *real* detail to preserve at low
        // resolution, so lowering λ over-codes there (CIF mobile/bus paid PSNR that equally-
        // active 1080p didn't). Floor the activity λ at `res_mult` (0.0007 CIF → 0.0005 1080p)
        // — the full model is λ(activity, resolution): activity picks λ, resolution floors it.
        // Only bites low-res high-activity blocks; akiyo/foreman/1080p are unchanged.
        if std::env::var("VP9_LAMBDA_NOFLOOR").is_ok() {
            act_mult
        } else {
            act_mult.max(res_mult)
        }
    }

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
        // Per-frame temporal activity = mean |source − reference| over the luma (stride-4
        // subsample) — the ZEROMV residual energy, a motion/detail proxy that drives the
        // activity-adaptive λ. 0 on key frames (no reference → treated as low activity).
        let activity = ref_planes.as_ref().map_or(0.0, |rp| {
            let (s, r) = (&src[0].buf, &rp[0].buf);
            let n = s.len().min(r.len());
            let (mut acc, mut cnt) = (0u64, 0u64);
            let mut i = 0;
            while i < n {
                acc += (s[i] as i32 - r[i] as i32).unsigned_abs() as u64;
                cnt += 1;
                i += 4;
            }
            if cnt > 0 { acc as f64 / cnt as f64 } else { 0.0 }
        });
        if std::env::var("VP9_ACT_DEBUG").is_ok() {
            eprintln!("ACT activity={:.3} inter={}", activity, is_inter);
        }
        let dc_y = dc_quant(qindex as i32, 8);
        let ac_y = ac_quant(qindex as i32, 8);
        let mut fe = FrameEncoder {
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
            aq: std::env::var("VP9_AQ").ok().and_then(|v| v.parse().ok()).unwrap_or(0),
            aq_active: false,
            aq_seg: Vec::new(),
            aq_ncols: 0,
            aq_dq_y: [(dc_y, ac_y); 2],
            aq_dq_uv: [(dc_quant(qindex as i32, 8), ac_quant(qindex as i32, 8)); 2],
            aq_lambda: [0.0; 2],
            max_px: 255,
            is_inter,
            refs: [ref_planes, None, None],
            src8: Vec::new(), // built below once `src` is in place
            refs8: std::cell::RefCell::new([None, None, None]),
            active_ref: 0,
            refresh_frame_flags: 1, // refresh LAST (slot 0) by default
            show_frame: true,
            ref_frame_idx: [0, 1, 2],
            // Frame-header interp filter. DEFAULT = 4 (SWITCHABLE: per-block filter RD
            // search — a −1.58% BD-rate win at speed 3, −0.90% at speed 0, decoder-ready).
            // `VP9_INTERP_FILTER=N` pins a fixed frame filter (0=EIGHTTAP/1=SMOOTH/2=SHARP,
            // the ceiling probe); `VP9_NO_SWITCHABLE` escapes to fixed EIGHTTAP.
            interp_filter: if let Some(f) = std::env::var("VP9_INTERP_FILTER")
                .ok()
                .and_then(|v| v.parse().ok())
            {
                f
            } else if std::env::var("VP9_NO_SWITCHABLE").is_ok() {
                0
            } else {
                4
            },
            active_filter: 0, // fixed up after construction (frame filter, or 0 if switchable)
            sign_bias: [false; 4], // all same ⇒ no compound, reference_mode forced single
            fc: FrameContext::defaults(),
            use_rdo: true,
            // Rate-distortion multiplier `ac²·mult` for `J = SSE + lambda·bits`,
            // content-activity-adaptive (`Self::lambda_mult`). `VP9_LAMBDA_MULT` overrides.
            lambda: (ac_y as f64)
                * (ac_y as f64)
                * Self::lambda_mult(activity, is_inter, width, height),
            lf_level: 0,
            counts: FrameCounts::zeroed(),
            commit_fc: None,
            // Forward coefficient-prob update (R4 "two-pass"): ON, RD-GATED per prob
            // (`rd_gate_coef_update`). Ungated adaptation measured a 20–28% inter
            // INFLATION (hundreds of subexp deltas each saving <1 token bit); the
            // per-prob savings gate keeps only paying deltas, so the update can now
            // only shrink the stream. `VP9_NO_2PASS=1` skips the gather pass entirely.
            use_prob_updates: std::env::var("VP9_NO_2PASS").is_err(),
            // R5 AC deadzone: OFF (4 = round-to-nearest). It lowered the encoder's
            // own RD cost J ~1.6%, but the unbiased BD-rate oracle
            // (`encode::quality`) proved it's a **+1.66% LOSS** (sheds bitrate for
            // far more PSNR) — the J self-metric flattered a loss. Kept as the
            // worked example of why a video RD knob needs the BD-rate gate.
            ac_round_num: 4,
            trellis_lambda_scale: std::env::var("VP9_TRELLIS_LAMBDA")
                .ok()
                .and_then(|v| v.parse().ok())
                .unwrap_or(1.0),
            // 2.5 = the content-adaptive sweet spot (mean −4.19% BD, all clips win, 32/32
            // conformant); k≈4 over-trims and can desync at extreme λ, so it's capped here.
            lf_from_q: std::env::var("VP9_LF_FROM_Q").is_ok(),
            trellis_k: std::env::var("VP9_TRELLIS_K")
                .ok()
                .and_then(|v| v.parse().ok())
                .unwrap_or(2.5),
            // R5 — ON: BD-rate oracle scores it −0.45% at the calibrated λ (a real
            // win; the same knob was a +40% catastrophe at the old too-high λ).
            use_trellis: std::env::var("VP9_NO_TRELLIS").is_err(),
            trellis_exact: std::env::var("VP9_TRELLIS_EXACT").is_ok(),
            // Roof — ON: BD-rate oracle scores it −18% (an 8×8 transform decorrelates
            // smooth residual far better than four 4×4s — fewer bits AND higher PSNR).
            use_tx_search: std::env::var("VP9_NO_TXSEARCH").is_err(),
            tx_order: std::env::var("VP9_NO_TXORDER").is_err(),
            tx_thresh: std::env::var("VP9_TX_THRESH")
                .ok()
                .and_then(|v| v.parse().ok())
                .unwrap_or(0.0),
            tx4x4: std::env::var("VP9_TX4X4").is_ok(),
            tx_cap: std::env::var("VP9_TXCAP")
                .ok()
                .and_then(|s| s.parse().ok())
                .unwrap_or(1),
            intra_gate: std::env::var("VP9_NO_INTRA_GATE").is_err(),
            intra_gate_t: std::env::var("VP9_INTRA_GATE_T")
                .ok()
                .and_then(|s| s.parse().ok())
                .unwrap_or(1000.0),
            mode_shortlist: std::env::var("VP9_NO_SHORTLIST").is_err(),
            shortlist_k: std::env::var("VP9_SHORTLIST_K")
                .ok()
                .and_then(|s| s.parse().ok())
                .unwrap_or(3),
            force_min_bsize: BLOCK_8X8, // only used when partition RD is off
            // Sub-8×8 (4×4/8×4/4×8) inter prediction: on by default (conformant, a BD-rate
            // win); `VP9_NO_SUB8X8` disables it (faster encode).
            sub8x8: std::env::var("VP9_NO_SUB8X8").is_err(),
            // Roof — ON: BD-rate oracle scores recursive partitioning −37% vs all-8×8
            // (large blocks are far cheaper on smooth content; it can always fall back
            // to 8×8 on detail). Key-frame only for now — inter stays all-8×8.
            use_partition_rd: true,
            disable_lf: std::env::var("VP9_NO_LF").is_ok(),
            pending_eob: 0,
            trial_abort_at: None,
            skip_trial: false,
            part_map: std::collections::HashMap::new(),
            mode_map: ModeMap::new(),
            last_trial_tx: 0,
            pending_pred_sse: 0,
            force_skip: false,
            rd_skip: std::env::var("VP9_NO_RD_SKIP").is_err(),
            // Activity-gated partition-merge bias. On near-static frames we over-split the
            // background into small skip blocks (akiyo: 84 8×8/frame vs libvpx's 21) —
            // splitting to code noise-level residual libvpx skips as one large block. Bias
            // SPLIT-vs-NONE toward the large skip block, but ONLY for near-static content
            // (the same signal the λ uses); any real motion keeps fine partitions. Env
            // `VP9_SPLIT_PEN` overrides with a fixed value.
            split_penalty: std::env::var("VP9_SPLIT_PEN")
                .ok()
                .and_then(|s| s.parse().ok())
                .unwrap_or_else(|| {
                    if is_inter {
                        // activity 2 (akiyo) → 1.05; activity ≥ 5 (foreman+) → 1.0 (off).
                        let t = ((5.0 - activity) / (5.0 - 2.0)).clamp(0.0, 1.0);
                        1.0 + t * 0.05
                    } else {
                        1.0
                    }
                }),
            full_msearch: std::env::var("VP9_DIAMOND_MSEARCH").is_err(),
            msearch_x4: std::env::var("VP9_NO_MSEARCH_X4").is_err(),
            corner_sad: std::env::var("VP9_CORNER_SAD").is_ok(),
            // 48 == NEWMV_SAD_PENALTY is PROVABLY lossless (a searched MV pays SAD+48).
            // Default 64 is the aggressive child: harvest showed <=0.8% of NEWMV wins
            // lost, BD-gated NEUTRAL (+0.02%) on the corpus ladder, ~-15% encode time.
            sub8x8_prescreen: std::env::var("VP9_SUB8X8_PRESCREEN")
                .ok()
                .and_then(|v| v.parse().ok())
                .unwrap_or(64),
            sub8_probe: std::env::var("VP9_SUB8_PROBE").is_ok(),
            lfseg_probe_on: std::env::var("VP9_LFSEG_PROBE").is_ok(),
            sub8x8_multiref: std::env::var("VP9_NO_SUB8X8_MULTIREF").is_err(),
            sub8x8_ref_penalty: std::env::var("VP9_SUB8X8_REF_PENALTY")
                .ok()
                .and_then(|v| v.parse().ok())
                .unwrap_or(256.0),
            sub8x8_multiref_gate: std::env::var("VP9_SUB8X8_MULTIREF_GATE")
                .ok()
                .and_then(|v| v.parse().ok())
                .unwrap_or(500.0),
            subpel_fast: false,
            motion_fast: std::env::var("VP9_MOTION_FAST").is_ok(),
            subpel_bilinear: std::env::var("VP9_SUBPEL_BILINEAR").is_ok()
                || std::env::var("VP9_SUBPEL_8TAP").is_err() && false, // set by presets
            subpel_tree: std::env::var("VP9_SUBPEL_TREE").is_ok(),
            subpel_diag: std::env::var("VP9_SUBPEL_DIAG").is_ok(),
            hp_mv: std::env::var("VP9_NO_HP_MV").is_err(),
            compound: std::env::var("VP9_COMPOUND").is_ok()
                || std::env::var("VP9_COMPOUND_FORCE").is_ok(),
            compound_force: std::env::var("VP9_COMPOUND_FORCE").is_ok(),
            no_compound: std::env::var("VP9_NO_COMPOUND").is_ok(),
            altref_future: false,
            compound_gate: std::env::var("VP9_COMPOUND_GATE")
                .ok()
                .and_then(|v| v.parse().ok())
                .unwrap_or(0.0),
            // Default-ON with the cost penalty that makes it a net win (mobile −6.57→−7.19%,
            // bus −1.55→−1.97%). `VP9_NO_COMPOUND_NEAR` opts out.
            compound_near: std::env::var("VP9_NO_COMPOUND_NEAR").is_err(),
            compound_filter: std::env::var("VP9_NO_COMPOUND_FILTER").is_err(),
            compound_joint: std::env::var("VP9_NO_COMPOUND_JOINT").is_err(),
            compound_near_penalty: std::env::var("VP9_COMPOUND_NEAR_PEN")
                .ok()
                .and_then(|v| v.parse().ok())
                .unwrap_or(24.0),
            trellis_trials: std::env::var("VP9_FAST_TRIALS").is_err(),
            trial_recon: std::env::var("VP9_TRIAL_RECON").is_ok(),
            split_early: std::env::var("VP9_SPLIT_EARLY").is_ok(),
            mode_thresh_mult: std::env::var("VP9_MODE_THRESH")
                .ok()
                .and_then(|v| v.parse().ok())
                .unwrap_or(0.0),
            g1_64: std::env::var("VP9_G1_64")
                .ok()
                .and_then(|v| v.parse().ok())
                .unwrap_or(0.0),
            trellis_frozen: std::env::var("VP9_TRELLIS_EXACT_CTX").is_err(),
            trellis_mse_t: std::env::var("VP9_TRELLIS_MSE_T")
                .ok()
                .and_then(|v| v.parse().ok())
                .unwrap_or(0.0),
            emit_dedup: std::env::var("VP9_DEDUP").is_ok(),
            dedup_map: std::cell::RefCell::new(std::collections::HashMap::new()),
            subpel_rounds: std::env::var("VP9_SUBPEL_ROUNDS")
                .ok()
                .and_then(|v| v.parse().ok())
                .unwrap_or(0),
            g1_harvest: std::env::var("VP9_G1_HARVEST").is_ok(),
            tile_start: 0,
            tile_end: mi_cols,
            chain: false,
            prev_mvs: None,
            tile_cols_log2: {
                let sb_cols = mi_cols.div_ceil(8);
                let mut l2: u32 = std::env::var("VP9_TILE_COLS")
                    .ok()
                    .and_then(|v| v.parse().ok())
                    .unwrap_or(if sb_cols >= 16 { 2 } else { 0 });
                while l2 > 0 && (sb_cols >> l2) < 4 {
                    l2 -= 1; // spec: min tile width 4 SBs
                }
                l2
            },
            g1_gate: std::env::var("VP9_NO_G1GATE").is_err(),
            g3_gate: std::env::var("VP9_G3GATE").is_ok(), // harvest-first: off by default
            g1_scale: 1.0,
            g1_area: 1.0,
            last_none_sse: 0,
            var_part: std::env::var("VP9_VAR_PART").is_ok(),
            var_thresh_mult: std::env::var("VP9_VAR_THRESH")
                .ok()
                .and_then(|v| v.parse().ok())
                .unwrap_or(24.0),
            vt: None,
            dispatch: std::env::var("VP9_DISPATCH").is_ok(),
            dispatch_thresh: std::env::var("VP9_DISPATCH_T")
                .ok()
                .and_then(|v| v.parse().ok())
                .unwrap_or(3000),
            dispatch_fixed_t: std::env::var("VP9_DISPATCH_T").is_ok(),
            dispatch_q: std::env::var("VP9_DISPATCH_Q")
                .ok()
                .and_then(|v| v.parse().ok())
                .unwrap_or(0.5),
            dispatch_hi: std::env::var("VP9_DISPATCH_HI").is_ok(),
            decision_us: 0,
            nonrd_leaf: std::env::var("VP9_NONRD_LEAF").is_ok(),
            variance_leaf: false,
            // Default 1.0 SAD/px: BD-neutral (−0.05%) at ~1.05× (1.14× on static content
            // where the gate fires often). Only ever active on the nonrd leaf (gated), so
            // harmless at the RD tiers. `VP9_NONRD_ME_SKIP=0` disables.
            nonrd_me_skip: std::env::var("VP9_NONRD_ME_SKIP")
                .ok()
                .and_then(|v| v.parse().ok())
                .unwrap_or(1.0),
            model_skip: std::env::var("VP9_MODEL_SKIP").is_ok(),
            model_skip_t: std::env::var("VP9_MODEL_SKIP_T")
                .ok()
                .and_then(|v| v.parse().ok())
                .unwrap_or(23),
            chroma_rd: std::env::var("VP9_CHROMA_RD").is_ok(), // set_speed enables for speed ≤ 3
            chroma_rd_w: std::env::var("VP9_CHROMA_RD_W")
                .ok()
                .and_then(|v| v.parse().ok())
                .unwrap_or(0.35), // tuned optimum (BD-rate-swept @s3)
            yuv_abort: std::env::var("VP9_NO_YUV_ABORT").is_err(),
            snap_recon: std::env::var("VP9_NO_SNAP_RECON").is_err(),
            model_rank: std::env::var("VP9_MODEL_RANK").is_ok(),
            tx_scratch: Box::default(),
            tx_memset: std::env::var("VP9_TX_MEMSET").is_ok(),
            has_avx2: {
                #[cfg(target_arch = "x86_64")]
                {
                    std::is_x86_feature_detected!("avx2")
                }
                #[cfg(not(target_arch = "x86_64"))]
                {
                    false
                }
            },
        };
        // The MC filter defaults to the fixed frame filter; a switchable frame (4)
        // starts the mode search at EIGHTTAP(0) and the per-block search refines it.
        // Resolution-aware G1 gate. `VP9_G1_AREA` is the exponent alpha; unset (0.0)
        // reproduces the fixed-threshold behaviour byte-for-byte. CIF (352x288) is the
        // reference area because the gate's percentiles were swept on CIF content.
        {
            const CIF_PX: f64 = 352.0 * 288.0;
            let alpha = env_f64("VP9_G1_AREA").unwrap_or(0.0);
            fe.g1_area = if alpha == 0.0 {
                1.0
            } else {
                ((width * height) as f64 / CIF_PX).max(1.0).powf(alpha)
            };
        }
        fe.active_filter = if fe.interp_filter < 4 { fe.interp_filter as u8 } else { 0 };
        // u8 search mirror of the luma source (values are exact for 8-bit).
        if fe.max_px == 255 {
            fe.src8 = fe.src[0].buf.iter().map(|&v| v as u8).collect();
        }
        fe
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
        self.refs8.borrow_mut()[1] = None;
    }

    /// Install the ALTREF reference (slot 2) — a (usually hidden, future) frame.
    pub fn set_altref(&mut self, recon: [Vec<u16>; 3]) {
        self.refs[2] = Some(self.ref_planes_from(recon));
        self.refs8.borrow_mut()[2] = None;
        self.altref_future = true; // ARF = a future reference (true bi-prediction)
    }

    /// Enable compound for this frame (the outer encoder turns it on for a whole ARF
    /// group so reference_mode stays consistent). Respects the `VP9_NO_COMPOUND` opt-out.
    pub fn set_compound(&mut self, on: bool) {
        if !self.no_compound {
            self.compound = on;
        }
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

    /// F3: chain this frame onto `fc` (the decoder's adapted context) with the
    /// previous frame's MV records for temporal prediction (None when the
    /// previous frame was a key/none — mirroring the decoder's `use_prev_mvs`).
    pub fn set_chain(
        &mut self,
        fc: FrameContext,
        prev_mvs: Option<std::sync::Arc<Vec<MvRef>>>,
    ) {
        self.fc = fc;
        self.prev_mvs = prev_mvs;
        self.chain = true;
    }

    /// The previous frame's MV record at this mi position (decoder's `prev_mv`).
    fn prev_mv(&self, mi_row: usize, mi_col: usize) -> Option<&MvRef> {
        self.prev_mvs
            .as_ref()
            .map(|g| &g[mi_row * self.mi_cols + mi_col])
    }

    /// Toggle R4 forward coefficient-probability updates (default on).
    #[cfg(test)]
    pub fn set_use_prob_updates(&mut self, v: bool) {
        self.use_prob_updates = v;
    }

    /// Apply an encoder speed preset (`-cpu-used`/`-speed`, 0 best..4 fastest) by
    /// progressively dropping RD tools, in *worst time-per-bit-saved order first*
    /// (measured by ablation on the Derf corpus). Level 0 is the quality anchor.
    /// (The forward-prob two-pass is already default-off — it was a net loss — so it
    /// is not part of the ladder.) Env `VP9_NO_*` overrides still apply on top.
    pub fn set_speed(&mut self, speed: u32) {
        if speed >= 1 {
            self.sub8x8 = false; // ~25% of encode for ~2% of bits — the worst trade
            self.full_msearch = false; // diamond search: ~1.2× for +2% BD-rate
            self.subpel_fast = true; // iterative ¼-pel refinement (≤ ~10 vs 24 scores)
            self.g1_scale = 2.0; // partition gate ×2 (swept: ~50% 8×8 skip, 92%+ gain kept)
        }
        if speed >= 2 {
            self.use_tx_search = false; // fix the transform size (skip the per-block search)
            self.use_prob_updates = false; // skip the token-count gather pass
            self.g1_scale = 4.0; // partition gate ×4
            self.motion_fast = true; // subsample mode-shortlist SSE (≤±0.03 dB, ~1.05×)
            // Bilinear-scored subpel refinement (libvpx sub_pixel_tree semantics):
            // BD +0.55% mean for a cheaper scorer; `VP9_SUBPEL_8TAP` restores the
            // commit-grade 8-tap scorer.
            self.subpel_bilinear = std::env::var("VP9_SUBPEL_8TAP").is_err();
            // Plus+diagonal single-pass subpel (8.3 scores/search vs 12.9,
            // subpel 2.4→1.2µs — BELOW libvpx's 1.75): BD +1.35% mean, the
            // speed-first trade; `VP9_SUBPEL_WALK` restores the iterating diamond.
            self.subpel_diag = std::env::var("VP9_SUBPEL_WALK").is_err();
        }
        if speed >= 4 {
            // Loop-filter level from a closed form instead of the 14-evaluation
            // search — libvpx makes the same call (LPF_PICK_FROM_Q at faster
            // cpu-used). GATED on a real BD run, 4 clips x 4 CRFs x 50 frames,
            // with rate and quality measured over the SAME frames:
            //   akiyo +0.178%  foreman +0.008%  bus +0.002%  mobile +0.381%
            //   MEAN  +0.142% BD  for ~+4.7% mean encode speed
            // (An earlier run read +0.155% before `-frames:v` existed and the
            // harness was pairing 120-frame rate with 50-frame PSNR; the
            // corrected number is essentially the same, but the old one was not
            // measuring what it claimed.)
            //
            // Deliberately NOT default at the quality tiers: our BD gap to
            // libvpx is the scarce resource there, so trading 0.155% BD for
            // ~5% speed only pays where speed is the objective. `VP9_LF_FROM_Q`
            // forces it on at any tier; `VP9_LF_SEARCH` forces the search.
            self.lf_from_q = std::env::var("VP9_LF_SEARCH").is_err();
        }
        if speed >= 3 {
            // Rebuilt from libvpx's cpu3-4 speed-features (the old rung dropped
            // the trellis: +14% size for ~5% speed — broken, removed). Screened
            // per-knob: escalating g1_scale was the toxic lever (8.0 alone ≈
            // +18% BD — the partition gate prunes where split matters); the
            // ladder leans on the mild ones instead.
            self.split_early = true; // abort losing SPLIT recursions early (~free)
            self.mode_thresh_mult = env_f64("VP9_MODE_THRESH").unwrap_or(1.25);
            // DISABLED (0), 2026-07-26. This gate shipped at 450 on the strength of a
            // 450-vs-900 sweep over four CIF clips (akiyo/foreman/bus/mobile, mean
            // -0.556% BD). That sweep never compared against OFF, and its corpus held
            // no high-motion HD, so it could not see either of the following.
            //
            // 1. It is a catastrophe on fast-motion HD. park_joy_1080p50 at speed 3:
            //    15.96 dB with the gate, 31.24 dB without — a 15.3 dB loss for +0.02%
            //    bytes. Frames 1-12 hold ~32.5 dB, frame 13 falls to 18.75, then decays
            //    to 12.86. It fires on just 2.9% of 64x64 nodes (harvested at qindex
            //    160), but a damaged block feeds the next frame's prediction, so the
            //    error compounds instead of staying local.
            //
            //    Mechanism: the gate's feature is `none_rd/lambda` = `SSE/lambda + bits`.
            //    lambda grows with q^2, so at high q the SSE term vanishes and the test
            //    degenerates into "did NONE code CHEAPLY" — firing hardest on SKIP
            //    blocks, i.e. exactly the blocks whose prediction was given up on.
            //
            //    A per-pixel SSE guard on the NONE arm was tried and NOT shipped: the
            //    response was non-monotonic (cap 50 -> 31.3 dB, but 25 -> 18.1 and
            //    100 -> 16.0), and the outcome is bimodal at near-identical byte counts.
            //    That is a fragile state divergence, not a smooth RD trade, so no cap
            //    constant is defensible without understanding it. `last_none_sse` is
            //    left plumbed for whoever picks that up.
            //
            // 2. It does not even pay on its own tuning corpus. BD-rate of OFF vs 450,
            //    4 CRFs x whole clips, rate and PSNR over the same frames:
            //      akiyo -0.507%  foreman -0.384%  bus +0.086%  mobile +0.000%
            //      MEAN  -0.201%  (negative = turning it OFF is BETTER)
            //    Better or neutral on three of four, and its own note priced the speed
            //    it bought at ~0.4%.
            //
            // `VP9_G1_64=450` restores the old behaviour for anyone re-investigating.
            self.g1_64 = env_f64("VP9_G1_64").unwrap_or(0.0);
            self.subpel_tree = true; // skip ¼-ring when ½-ring didn't move
            self.intra_gate_t = env_f64("VP9_INTRA_GATE_T").unwrap_or(2000.0);
            // Lever 1 (a MILD fixed-percentile dispatch at this default tier) was
            // tried and REVERTED: BD-rate refuted it. A single-CRF PSNR looked ~free
            // (mobile −0.03 dB), but over the full RD ladder it cost +2.29% mean
            // BD-rate for only ~1.1× — and was a PURE LOSS on easy content (akiyo
            // +0.93% BD-rate AND 0.97× i.e. slower), because a fixed percentile
            // routes the flat-but-cheap SBs that RD handles cheaply anyway (quality
            // lost, no time saved). The default tier stays quality-optimal (RD-only).
            // The content-invariant speedup lives in Lever 2 (the `VP9_DISPATCH_BUDGET`
            // time-budget controller in the outer encoder), which routes only what a
            // per-frame time target needs — +1.2% BD @30 ms for up to 1.7× on complex
            // clips while easy content stays full-RD (−1.1%). `VP9_DISPATCH` still
            // enables the manual fixed-q dispatch here for sweeps.
        }
        // speeds 4+ are the REALTIME rungs the old ladder couldn't reach with
        // thresholds alone: the content-adaptive dispatcher (Bricks 1–4) routes a
        // rising fraction `q` of each frame's superblocks through the O(pixels)
        // variance partition instead of the recursive RD search — the SBs where RD
        // buys nothing (flat/well-predicted). `q` is a PERCENTILE of the frame's own
        // variance distribution, so the routed fraction — and thus the speed — is
        // content-invariant: the lever that flattens the ~5× content-speed variance
        // the threshold ladder could not. `VP9_DISPATCH_Q` pins `q` for sweeps.
        if speed >= 4 {
            self.dispatch = true;
            // Non-RD leaf on the dispatcher's fast arm: variance-routed leaves take the
            // cheaper LAST-ref-only + forced-model-skip decision. A Pareto improvement
            // to these tiers — ~1.1× faster at neutral-or-better BD-rate (s5 −0.26%,
            // s6 −0.10% mean; akiyo −1.6% where model-skip helps most). `VP9_NO_NONRD_LEAF`
            // disables. See the non-RD leaf field doc.
            if std::env::var("VP9_NO_NONRD_LEAF").is_err() {
                self.nonrd_leaf = true;
            }
            if std::env::var("VP9_DISPATCH_Q").is_err() {
                self.dispatch_q = match speed {
                    4 => 0.50, // ~1.4× on busy content, near-neutral quality
                    5 => 0.75, // ~1.8×
                    _ => 0.90, // speed 6+: ~2.2×, approaching the all-variance floor
                };
            }
        }
        // Chroma-aware mode RD (`rd_cost_yuv`): the inter/intra pick minimises luma +
        // 0.35·chroma RD instead of luma alone. A −0.30% BD-rate win @s3 (akiyo −0.83%,
        // mobile −0.38%) for ~1.18× encode, so a COMPRESSION-tier default (speed ≤ 3);
        // the realtime rungs (≥4) stay fast. `VP9_CHROMA_RD`/`VP9_NO_CHROMA_RD` override.
        self.chroma_rd = if std::env::var("VP9_CHROMA_RD").is_ok() {
            true
        } else if std::env::var("VP9_NO_CHROMA_RD").is_ok() {
            false
        } else {
            speed <= 3
        };
    }

    /// Lever 2 — force the content-adaptive dispatch on at an externally-chosen
    /// route fraction `q`. The outer encoder's time-budget controller calls this
    /// each frame with the `q` it has adapted from the previous frames' decision
    /// times (overriding whatever `set_speed` picked), since the controller's
    /// cross-frame state can't live in a per-frame `FrameEncoder`.
    pub(crate) fn set_dispatch_q(&mut self, q: f64) {
        self.dispatch = true;
        self.dispatch_q = q.clamp(0.0, 1.0);
        // The budget controller uses the variance path for speed, so its fast arm gets
        // the non-RD leaf too (Pareto: ~1.1× faster at neutral BD). `VP9_NO_NONRD_LEAF` off.
        if std::env::var("VP9_NO_NONRD_LEAF").is_err() {
            self.nonrd_leaf = true;
        }
    }

    /// Lever 2 — the decision-pass wall time (µs) the last `encode_frame` spent,
    /// the feedback signal the outer time-budget controller steers `q` on.
    pub(crate) fn decision_us(&self) -> u64 {
        self.decision_us
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
        let _s = prof::Scope::new(prof::S::SnapRestore);
        let (x0, y0, bw, bh) = self.block_px(mi_row, mi_col, bwl, bhl, 0);
        let cw = self.rec[0].stride;
        // Vec + row memcpys: the fixed [u16; 4096] zero-initialized 8 KB per
        // snapshot (GBs of memset per encode) and copied per-pixel.
        let mut y = Vec::with_capacity(bw * bh);
        for r in 0..bh {
            let row = (y0 + r) * cw + x0;
            y.extend_from_slice(&self.rec[0].buf[row..row + bw]);
        }
        let mut above = [0u8; 16];
        let aw = bw / 4; // in-frame 4×4-columns the block spans
        above[..aw].copy_from_slice(&self.above_ctx[0][mi_col * 2..mi_col * 2 + aw]);
        (y, above, self.left_ctx[0])
    }

    fn restore_y(&mut self, mi_row: usize, mi_col: usize, bwl: usize, bhl: usize, snap: &YSnap) {
        let _s = prof::Scope::new(prof::S::SnapRestore);
        let (x0, y0, bw, bh) = self.block_px(mi_row, mi_col, bwl, bhl, 0);
        let cw = self.rec[0].stride;
        for r in 0..bh {
            let row = (y0 + r) * cw + x0;
            self.rec[0].buf[row..row + bw].copy_from_slice(&snap.0[r * bw..r * bw + bw]);
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
        best_so_far: f64,
    ) -> f64 {
        let _s = prof::Scope::new(prof::S::RdCost);
        // Early abort: J accumulates monotonically over tx blocks (sse and bits
        // are non-negative integer sums; int→f64 and +/× by λ>0 are monotone),
        // so once the running J strictly exceeds the incumbent the candidate has
        // PROVABLY lost — the exact final value can't change the decision. The
        // trial state is restored below either way.
        let abort_at = if self.use_rdo && best_so_far.is_finite() {
            Some(best_so_far - self.lambda * extra_bits)
        } else {
            None
        };
        self.trial_abort_at = abort_at;
        let (bits_q8, sse) = self.encode_plane(None, mi, 0, mi_row, mi_col, bsize, bwl, bhl);
        self.trial_abort_at = None;
        self.restore_y(mi_row, mi_col, bwl, bhl, snap);
        if bits_q8 == u64::MAX {
            return f64::INFINITY; // aborted: strictly worse than the incumbent
        }
        let rate = if self.use_rdo {
            self.lambda * (bits_q8 as f64 / 256.0 + extra_bits)
        } else {
            0.0
        };
        sse as f64 + rate
    }

    /// Chroma-aware candidate RD: like `rd_cost_y` but costs all three planes
    /// (luma + both chroma) so the mode/MV pick minimises the JOINT luma+chroma RD.
    /// Snapshots/restores the whole block (chroma recon + contexts too). No early
    /// abort (the running J spans planes) — a `chroma_rd` speed-tier feature.
    #[allow(clippy::too_many_arguments)]
    fn rd_cost_yuv(
        &mut self,
        mi: &ModeInfo,
        mi_row: usize,
        mi_col: usize,
        bsize: usize,
        bwl: usize,
        bhl: usize,
        extra_bits: f64,
        best_so_far: f64,
    ) -> f64 {
        let _s = prof::Scope::new(prof::S::RdCost);
        let snap = self.snap_block(mi_row, mi_col, bwl, bhl);
        // Luma first, then a bound check. Chroma only ADDS to J (SSE/bits ≥ 0), so once the
        // luma-only J exceeds the incumbent this candidate has provably lost — the chroma
        // trial reconstruct is pure waste. Abort: byte-identical decision (a winner has
        // J ≤ best_so_far and never aborts, so the committed recon is unchanged), skips ~⅔
        // of the trial planes for losers — the biggest chunk of rd_cost wrapper overhead.
        let (y_bits, y_sse) = self.encode_plane(None, mi, 0, mi_row, mi_col, bsize, bwl, bhl);
        let rate0 = if self.use_rdo {
            self.lambda * (y_bits as f64 / 256.0 + extra_bits)
        } else {
            0.0
        };
        let j_luma = y_sse as f64 + rate0;
        if self.yuv_abort && j_luma > best_so_far {
            self.restore_block(mi_row, mi_col, bwl, bhl, &snap);
            return j_luma;
        }
        let (mut c_bits, mut c_sse) = (0u64, 0u64);
        for plane in 1..3 {
            let (b, s) = self.encode_plane(None, mi, plane, mi_row, mi_col, bsize, bwl, bhl);
            c_bits += b;
            c_sse += s;
        }
        self.restore_block(mi_row, mi_col, bwl, bhl, &snap);
        // Chroma enters as a weighted tiebreaker (`chroma_rd_w`): full weight can tip a
        // borderline luma decision the wrong way on chroma-heavy content (bus lost luma
        // at full weight); a lighter weight keeps chroma as a discriminator without
        // overriding the luma-dominant pick.
        let w = self.chroma_rd_w;
        let sse = y_sse as f64 + w * c_sse as f64;
        let bits = y_bits as f64 / 256.0 + w * (c_bits as f64 / 256.0);
        let rate = if self.use_rdo {
            self.lambda * (bits + extra_bits)
        } else {
            0.0
        };
        sse + rate
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

    /// The tx size a candidate's RD trial (and the final coding, when the tx-size
    /// search is off) starts from. With the search ON, start at 4×4 and let
    /// `best_tx_size` refine up. With it OFF (fast presets), start at the block's
    /// MAX tx: 4×4 would transform the whole block per candidate (the flood) and
    /// code smooth residual with far too many small transforms. `VP9_TX4X4` pins 4×4.
    fn base_tx(&self, bsize: usize) -> u8 {
        if self.use_tx_search || self.tx4x4 {
            0
        } else {
            // Cap at 8×8 (tx_cap=1) by default: it reduces the 4×4 transform flood AND
            // improves compression, while staying speed-neutral. 16×16/32×32 are a
            // BD-rate win too but our large-tx fwd/trellis kernels are naive (super-
            // linear per call), so they'd be 2–3× SLOWER — capped out until optimized.
            (MAX_TXSIZE[bsize] as u8).min(self.tx_cap)
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
        // Charge each candidate the tx_size SYNTAX bits TX_MODE_SELECT will code
        // for it (1–3 bools at the real tx probs) — without this the search
        // "wins" residual bits it silently spends back on signaling (measured:
        // tx-search made static clips BIGGER). `VP9_NO_TXCOST` restores the
        // uncosted search (the A/B oracle).
        let charge = std::env::var("VP9_NO_TXCOST").is_err();
        let above = self.above_mi(mi_row, mi_col);
        let left = self.left_mi(mi_row, mi_col);
        let ctx = tx_size_context(mi, above.as_ref(), left.as_ref());
        let mut best = (0u8, f64::INFINITY);
        // Content-adaptive evaluation ORDER (byte-identical, `VP9_NO_TXORDER` restores
        // ascending): try the LIKELY-BEST size first so rd_cost_y's monotone-J abort prunes
        // the losers. A winner's running J never exceeds the incumbent ⇒ never aborts, so
        // the min-J size always wins (ties broken toward the SMALLER tx either way). Small-MV
        // / well-predicted blocks have smooth residual ⇒ favour the LARGE tx (descend);
        // high-motion blocks favour SMALL (the default ascend). Same work either direction
        // when nothing prunes; big prune when the first-tried size is the winner.
        let descend =
            self.tx_order && (mi.mv[0].0.abs() + mi.mv[0].1.abs()) <= 16 && mi.mode != NEWMV;
        for i in 0..=max_tx as u8 {
            let t = if descend { max_tx as u8 - i } else { i };
            let mut m = *mi;
            m.tx_size = t;
            let extra = if charge {
                self.tx_size_cost_q8(t, ctx, max_tx) as f64 / 256.0
            } else {
                0.0
            };
            let j = self.rd_cost_y(&m, mi_row, mi_col, bsize, bwl, bhl, snap, extra, best.1);
            if j < best.1 || (j == best.1 && t < best.0) {
                best = (t, j);
            } else if self.tx_thresh > 0.0 && best.1.is_finite() && j > best.1 * self.tx_thresh {
                // Confidence early-break: with likely-best-first order, a size that
                // loses to the incumbent by this factor means the remaining (further
                // from the predicted best) sizes won't win — stop the search.
                break;
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
        (mi_col > self.tile_start).then(|| self.mi[mi_row * self.mi_cols + mi_col - 1])
    }

    /// Code the single tile over the whole frame (resetting the per-tile entropy
    /// context) and return its bytes. Driven twice by `encode_frame` for R4.
    fn encode_tile(&mut self, tile_start: usize, tile_end: usize) -> Vec<u8> {
        self.tile_start = tile_start;
        self.tile_end = tile_end;
        let mut enc = BoolEncoder::new();
        for (p, c) in self.above_ctx.iter_mut().enumerate() {
            let ss = (p > 0) as usize;
            c[(tile_start * 2) >> ss..(tile_end * 2) >> ss]
                .iter_mut()
                .for_each(|v| *v = 0);
        }
        self.above_seg[tile_start..tile_end]
            .iter_mut()
            .for_each(|v| *v = 0);
        let mut mi_row = 0;
        while mi_row < self.mi_rows {
            self.left_seg = [0; 8];
            self.left_ctx = [[0; 16]; 3];
            let mut mi_col = tile_start;
            while mi_col < tile_end {
                self.set_sb_aq(mi_row, mi_col);
                self.encode_partition(&mut enc, mi_row, mi_col, BLOCK_64X64, 4);
                mi_col += 8;
            }
            mi_row += 8;
        }
        enc.finish()
    }

    /// Per-prob RD gate for the forward coefficient update (libvpx
    /// `vp9_cond_prob_diff_update`): revert `updated[..]` to the default wherever
    /// the delta's token savings (from the gathered branch counts) don't pay for
    /// its subexp signaling. All costs in Q8 bits via the same `cost_bit` table
    /// the RD path uses, so the decision is consistent with the emitted stream.
    fn rd_gate_coef_update(&self, defaults: &FrameContext, updated: &mut FrameContext) {
        // Q8 cost of the subexp delta body (each bool is at prob 128 = 256 Q8/bit).
        let subexp_q8 = |d: u32| -> u64 {
            let bits = if d < 16 {
                1 + 4
            } else if d < 32 {
                2 + 4
            } else if d < 64 {
                3 + 5
            } else if d - 64 < 65 {
                3 + 7
            } else {
                3 + 8
            };
            bits * 256
        };
        for tx in 0..4 {
            for i in 0..2 {
                for j in 0..2 {
                    for k in 0..6 {
                        let nctx = if k == 0 { 3 } else { 6 };
                        for l in 0..nctx {
                            let c = &self.counts.coef[tx][i][j][k][l];
                            let (n0, n1, n2, neob) = (c[0], c[1], c[2], c[3]);
                            let eob = self.counts.eob_branch[tx][i][j][k][l];
                            let branch = [[neob, eob - neob], [n0, n1 + n2], [n1, n2]];
                            for m in 0..3 {
                                let old = defaults.coef_probs[tx][i][j][k][l][m];
                                let new = updated.coef_probs[tx][i][j][k][l][m];
                                if new == old {
                                    continue;
                                }
                                let [c0, c1] = branch[m];
                                let save = (c0 as i64) * cost_bit(old, 0) as i64
                                    + (c1 as i64) * cost_bit(old, 1) as i64
                                    - (c0 as i64) * cost_bit(new, 0) as i64
                                    - (c1 as i64) * cost_bit(new, 1) as i64;
                                let d = super::prob::forward_remap_prob(new, old) as u32;
                                let sig = cost_bit(252, 1) as i64 - cost_bit(252, 0) as i64
                                    + subexp_q8(d) as i64;
                                if save <= sig {
                                    updated.coef_probs[tx][i][j][k][l][m] = old;
                                }
                            }
                        }
                    }
                }
            }
        }
    }

    /// Encode the frame and return the complete VP9 bitstream.
    pub fn encode_frame(&mut self) -> Vec<u8> {
        // Compound setup (before the decision pass reads sign_bias / reference_mode).
        // `self.compound` is enabled by the outer encoder for the WHOLE ARF group (or by
        // `VP9_COMPOUND`); a shown P frame that references the future ARF does TRUE
        // bi-prediction, the ARF frame / non-ARF P frames fall back to LAST+GOLDEN — so
        // reference_mode is consistent across the group (mixing single/compound across a
        // group desyncs libvpx, a self-tolerated-but-illegal stream).
        if self.is_inter && self.compound && !self.no_compound && self.altref_future && self.refs[2].is_some() {
            self.sign_bias = [false, false, false, true]; // INTRA,LAST,GOLDEN,ALTREF
            self.fc.reference_mode = 2; // REFERENCE_MODE_SELECT
            self.fc.comp_fixed_ref = 3; // ALTREF (future) is the fixed compound ref
            self.fc.comp_var_ref = [1, 2]; // LAST, GOLDEN
        } else if self.is_inter && self.compound && !self.no_compound && self.refs[1].is_some() {
            // Fallback (no future ARF, explicit VP9_COMPOUND): LAST+GOLDEN via the trick.
            self.sign_bias = [false, false, true, false];
            self.fc.reference_mode = 2;
            self.fc.comp_fixed_ref = 2; // GOLDEN
            self.fc.comp_var_ref = [1, 3]; // LAST, ALTREF
        } else {
            self.compound = false; // key frame / no usable second ref ⇒ no compound
        }
        // Activity-based ADAPTIVE QUANTIZATION: a per-SB variance pre-pass sorts each 64×64
        // into a low/high-activity segment (median split, content-invariant), each carrying
        // an ALT_Q qindex delta (seg0 = base−aq, seg1 = base+aq). Precompute per-segment
        // dequant + λ; the SB root sets dq_y/dq_uv/lambda from `aq_seg`. seg_id coded per block.
        if self.aq != 0 {
            let sb_rows = self.mi_rows.div_ceil(8);
            let sb_cols = self.mi_cols.div_ceil(8);
            self.aq_ncols = sb_cols;
            let mut vars: Vec<i64> = Vec::with_capacity(sb_rows * sb_cols);
            for sbr in 0..sb_rows {
                for sbc in 0..sb_cols {
                    self.build_vt(sbr * 8, sbc * 8);
                    vars.push(self.vt.as_ref().unwrap().variance(3, 0, 0));
                }
            }
            self.vt = None;
            let mut sorted = vars.clone();
            sorted.sort_unstable();
            let median = sorted[sorted.len() / 2];
            // Content gate: AQ (variance direction) is a SSIM win on low/mixed-activity
            // content but a loss on uniformly high-activity content — enable per frame only
            // when the frame's median SB variance is below the threshold.
            // ~8000 enables AQ on mostly-flat frames (akiyo inter ~1.4k) where it wins on
            // BOTH PSNR and SSIM, and disables it on textured content (foreman ~15k,
            // mobile ~130k) where it loses. Calibrated on the CIF set.
            let maxvar: i64 = std::env::var("VP9_AQ_MAXVAR")
                .ok()
                .and_then(|v| v.parse().ok())
                .unwrap_or(8000);
            if std::env::var("VP9_AQ_DEBUG").is_ok() {
                eprintln!("AQ median_var={} gate_max={} active={}", median, maxvar, median <= maxvar);
            }
            self.aq_active = median <= maxvar;
            if self.aq_active {
                self.aq_seg = vars.iter().map(|&v| (v > median) as u8).collect();
                let base_q = self.qindex as i32;
                let q0 = (base_q - self.aq).clamp(1, 255);
                let q1 = (base_q + self.aq).clamp(1, 255);
                self.aq_dq_y[0] = (dc_quant(q0, 8), ac_quant(q0, 8));
                self.aq_dq_y[1] = (dc_quant(q1, 8), ac_quant(q1, 8));
                self.aq_dq_uv[0] = self.aq_dq_y[0];
                self.aq_dq_uv[1] = self.aq_dq_y[1];
                let base_ac = self.dq_y.1 as f64;
                let mult = self.lambda / (base_ac * base_ac);
                self.aq_lambda[0] = (self.aq_dq_y[0].1 as f64).powi(2) * mult;
                self.aq_lambda[1] = (self.aq_dq_y[1].1 as f64).powi(2) * mult;
            }
        }
        let initial = self.fc.clone();
        let _prof = std::env::var("VP9_PROF").is_ok();
        let _t = std::time::Instant::now();
        if self.partition_rd_active() {
            // Brick 4: measure this frame's SB variance distribution and set the
            // content-adaptive dispatch threshold before the decision pass reads it.
            self.set_dispatch_threshold();
            // Choose every partition by RD first; the emit pass(es) below replay
            // the recorded decisions. Reset the recon the decision pass left behind.
            self.run_partition_decision();
            for p in self.rec.iter_mut() {
                p.buf.iter_mut().for_each(|v| *v = 0);
            }
        }
        self.decision_us = _t.elapsed().as_micros() as u64;
        if _prof {
            PROF_DECISION.fetch_add(self.decision_us, std::sync::atomic::Ordering::Relaxed);
        }
        let _t = std::time::Instant::now();
        if self.use_prob_updates {
            // R4 pass 1: code with default probs to gather the committed token
            // counts, then forward-adapt the coefficient probs toward them.
            self.counts = FrameCounts::zeroed();
            self.commit_fc = None;
            for t in 0..(1usize << self.tile_cols_log2) {
                let ts = tile_offset_enc(t, self.mi_cols, self.tile_cols_log2);
                let te = tile_offset_enc(t + 1, self.mi_cols, self.tile_cols_log2);
                let _ = self.encode_tile(ts, te);
            }
            let mut updated = initial.clone();
            adapt_coef_probs(
                &mut updated,
                &initial,
                &self.counts,
                COEF_COUNT_SAT,
                COEF_MAX_UPDATE_FACTOR,
            );
            // RD-GATE each adapted prob (libvpx `vp9_cond_prob_diff_update`): keep a
            // delta only when the token bits it saves exceed the bits it costs to
            // signal. Unconditional adaptation measured a 20–28% INFLATION on inter
            // frames (sparse residual: hundreds of subexp deltas, each saving <1
            // token bit); gating reverts those while keeping the dense-keyframe wins.
            self.rd_gate_coef_update(&initial, &mut updated);
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
        if _prof {
            PROF_EMIT1.fetch_add(_t.elapsed().as_micros() as u64, std::sync::atomic::Ordering::Relaxed);
        }
        let _t = std::time::Instant::now();
        // Final (or only) pass: code with the adapted probs if set, else defaults.
        let tiles: Vec<Vec<u8>> = (0..(1usize << self.tile_cols_log2))
            .map(|t| {
                let ts = tile_offset_enc(t, self.mi_cols, self.tile_cols_log2);
                let te = tile_offset_enc(t + 1, self.mi_cols, self.tile_cols_log2);
                self.encode_tile(ts, te)
            })
            .collect();
        let mut target = self.commit_fc.take().unwrap_or_else(|| initial.clone());
        if self.use_tx_search {
            target.tx_mode = 4; // TX_MODE_SELECT — per-block tx_size is coded
        } else if !self.tx4x4 {
            // Fast presets: no per-block tx bits, every block uses its capped max tx.
            // ALLOW_{n} makes the decoder DERIVE tx = min(max_tx[bsize], n) = exactly
            // `base_tx` — matching the residual we coded, zero tx syntax. The ALLOW
            // mode index equals its biggest tx, so tx_mode == tx_cap.
            target.tx_mode = self.tx_cap as usize; // 1=ALLOW_8X8 2=ALLOW_16X16 3=ALLOW_32X32
        }
        let tile_data = assemble_tiles(&tiles);

        if _prof {
            PROF_EMIT2.fetch_add(_t.elapsed().as_micros() as u64, std::sync::atomic::Ordering::Relaxed);
        }
        let _t = std::time::Instant::now();
        // ---- compressed header: signal the coef deltas; deblock the recon (R3) ----
        let mut h = self.frame_header();
        if self.aq_active {
            // AQ segmentation: seg0 = base−aq (finer, low-activity), seg1 = base+aq
            // (coarser, high-activity). Tree-only map (no temporal pred), fresh each frame.
            h.seg_enabled = true;
            h.seg_update_map = true;
            h.seg_temporal_update = false;
            h.seg_update_data = true;
            h.seg_abs_delta = false;
            h.seg_tree_probs = AQ_TREE_PROBS;
            h.seg_feature_enabled[0][0] = true; // ALT_Q, segment 0
            h.seg_feature_data[0][0] = -self.aq;
            h.seg_feature_enabled[1][0] = true; // ALT_Q, segment 1
            h.seg_feature_data[1][0] = self.aq;
        }
        self.apply_loop_filter(&mut h);
        let mut cenc = BoolEncoder::new();
        write_compressed_header(&mut cenc, &initial, &target, &h);
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
        if _prof {
            use std::sync::atomic::Ordering::Relaxed;
            PROF_HDR.fetch_add(_t.elapsed().as_micros() as u64, Relaxed);
            let (d, e1, e2, hd) = (
                PROF_DECISION.load(Relaxed),
                PROF_EMIT1.load(Relaxed),
                PROF_EMIT2.load(Relaxed),
                PROF_HDR.load(Relaxed),
            );
            let tot = (d + e1 + e2 + hd).max(1);
            eprintln!(
                "VP9_PROF cum(ms): decision={:.0} ({:.0}%)  emit1_gather={:.0} ({:.0}%)  emit2_final={:.0} ({:.0}%)  hdr+lf={:.0} ({:.0}%)",
                d as f64 / 1e3, d as f64 * 100.0 / tot as f64,
                e1 as f64 / 1e3, e1 as f64 * 100.0 / tot as f64,
                e2 as f64 / 1e3, e2 as f64 * 100.0 / tot as f64,
                hd as f64 / 1e3, hd as f64 * 100.0 / tot as f64,
            );
        }
        prof::dump();
        if std::env::var("VP9_IF_HARVEST").is_ok() {
            use std::sync::atomic::Ordering::Relaxed;
            let s0 = IF_HARVEST[0].load(Relaxed);
            let smin = IF_HARVEST[1].load(Relaxed);
            let n = IF_HARVEST[2].load(Relaxed).max(1);
            let wins = IF_HARVEST[3].load(Relaxed);
            eprintln!(
                "VP9_IF_HARVEST cum: blocks={} filter-wins={} ({:.1}%)  residual-SSE eighttap={} best={}  reduction={:.2}%",
                n, wins, 100.0 * wins as f64 / n as f64, s0, smin,
                100.0 * (s0.saturating_sub(smin)) as f64 / s0.max(1) as f64,
            );
        }
        if std::env::var("VP9_SUB8_PROBE").is_ok() {
            use std::sync::atomic::Ordering::Relaxed;
            let s0 = SUB8_PROBE[0].load(Relaxed);
            let smin = SUB8_PROBE[1].load(Relaxed);
            let n = SUB8_PROBE[2].load(Relaxed).max(1);
            let wins = SUB8_PROBE[3].load(Relaxed);
            eprintln!(
                "VP9_SUB8_PROBE cum: sub-blocks={} comp-wins={} ({:.1}%)  SAD single={} best={}  reduction={:.2}%",
                n, wins, 100.0 * wins as f64 / n as f64, s0, smin,
                100.0 * (s0.saturating_sub(smin)) as f64 / s0.max(1) as f64,
            );
        }
        if std::env::var("VP9_LFSEG_PROBE").is_ok() {
            use std::sync::atomic::Ordering::Relaxed;
            let g = LFSEG_PROBE[0].load(Relaxed);
            let c = LFSEG_PROBE[1].load(Relaxed);
            let n = LFSEG_PROBE[2].load(Relaxed).max(1);
            eprintln!(
                "VP9_LFSEG_PROBE cum: frames={} global-SSE={} per-SB-oracle-SSE={}  ceiling-reduction={:.2}%",
                n, g, c, 100.0 * (g.saturating_sub(c)) as f64 / g.max(1) as f64,
            );
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
            tile_cols_log2: self.tile_cols_log2,
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
            // Compound: the fixed ref gets the opposite sign bias. `compound_allowed` in
            // the compressed header keys on this, so the reference_mode bits are emitted.
            h.ref_sign_bias = if self.compound {
                if self.fc.comp_fixed_ref == 3 {
                    [false, false, true] // ALTREF fixed (true bi-prediction)
                } else {
                    [false, true, false] // GOLDEN fixed (LAST+GOLDEN fallback)
                }
            } else {
                [false, false, false]
            };
            h.allow_high_precision_mv = self.hp_mv;
            h.interp_filter = self.interp_filter;
            h.reset_frame_context = 0;
            // Error-resilient: each frame is independently decodable. This forces
            // `use_prev_frame_mvs = false` (we don't do temporal MV prediction — our
            // `find_mv_refs` passes `None`), disables backward adaptation (we only
            // forward-signal prob deltas), and codes every frame against the default
            // context. Without it a conformant decoder would use the previous P
            // frame's MVs as temporal candidates and diverge from frame 2 onward.
            h.error_resilient = !self.chain;
            if self.chain {
                // Context chaining: the decoder backward-adapts (refresh, not
                // frame-parallel) and uses temporal MVs — we mirrored both.
                h.refresh_frame_context = true;
                h.frame_parallel_decoding_mode = false;
                h.frame_context_idx = 0;
            }
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

        // NONE codes the whole block here; SPLIT recurses — EXCEPT at BLOCK_8X8, where
        // SPLIT (like HORZ/VERT) codes a single sub-8×8 leaf of `subsize` (4×4/8×4/4×8),
        // never a recursion. (Gated on the partition, NOT hbs — a forced-/RD-NONE at
        // 16×16+ must not fall through to recursion.)
        if partition == PARTITION_NONE || bsize == BLOCK_8X8 {
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

        // AQ: segment_id is the first per-block syntax (key-frame intra path).
        if self.aq_active {
            write_segment_id(enc, self.sb_seg(mi_row, mi_col), &AQ_TREE_PROBS);
        }
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
            tx_size: self.base_tx(bsize),
            ..Default::default()
        };
        let snap = self.snap_y(mi_row, mi_col, bwl, bhl);
        let mut best = (DC_PRED, f64::MAX);
        for &m in &[DC_PRED, V_PRED, H_PRED, TM_PRED] {
            mi.mode = m;
            let j = self.rd_cost_y(&mi, mi_row, mi_col, bsize, bwl, bhl, &snap, 0.0, best.1);
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
        let _sm = prof::Scope::new(prof::S::StoreMi);
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
        let _mc = prof::Scope::new(prof::S::MiCost);
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
        let _mc = prof::Scope::new(prof::S::MiCost);
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
                self.tile_start,
                self.tile_end,
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

    /// Signaling-bit cost (Q8) of a sub-8×8 block: skip + is_inter + single-ref (LAST),
    /// then each 4×4 sub-block's inter-mode symbol and NEWMV MV delta (relative to the
    /// shared `find_mv_refs(NEWMV)` predictor). No tx_size (sub-8×8 forces 4×4).
    fn sub8x8_modeinfo_cost_q8(&self, mi: &ModeInfo, mi_row: usize, mi_col: usize) -> u64 {
        let _mc = prof::Scope::new(prof::S::MiCost);
        let above = self.above_mi(mi_row, mi_col);
        let left = self.left_mi(mi_row, mi_col);
        let bsize = mi.sb_type as usize;
        let sctx = skip_context(above.as_ref(), left.as_ref());
        let mut c = cost_bit(self.fc.skip_probs[sctx], mi.skip as u32);
        let ictx = intra_inter_context(above.as_ref(), left.as_ref());
        c += cost_bit(self.fc.intra_inter_prob[ictx], mi.is_inter as u32);
        let ctx0 = single_ref_p1(above.as_ref(), left.as_ref());
        c += cost_bit(self.fc.single_ref_prob[ctx0][0], 0); // LAST
        let mctx = get_mode_context(
            &self.mi, self.mi_cols, self.mi_rows, self.tile_start, self.tile_end, mi_row, mi_col, bsize,
        );
        let num_4x4_w = 1usize << B_WIDTH_LOG2[bsize];
        let num_4x4_h = 1usize << B_HEIGHT_LOG2[bsize];
        let edges = self.block_edges(mi_row, mi_col, bsize);
        let (cand, _) = find_mv_refs(
            &self.mi, self.mi_cols, self.mi_rows, self.tile_start, self.tile_end, mi_row, mi_col, bsize,
            LAST_FRAME, &self.sign_bias, NEWMV, -1, edges, self.prev_mv(mi_row, mi_col),
        );
        let pred = lower_mv_precision(cand[0], self.hp_mv);
        let mut idy = 0;
        while idy < 2 {
            let mut idx = 0;
            while idx < 2 {
                let j = idy * 2 + idx;
                c += tree_bit_cost(
                    &INTER_MODE_TREE,
                    &self.fc.inter_mode_probs[mctx],
                    (mi.bmi[j] - NEARESTMV) as i32,
                );
                if mi.bmi[j] == NEWMV {
                    let dr = (mi.bmi_mv[j][0].0 - pred.0).unsigned_abs();
                    let dc = (mi.bmi_mv[j][0].1 - pred.1).unsigned_abs();
                    let bits = 10 + 2 * ((32 - dr.leading_zeros()) + (32 - dc.leading_zeros()));
                    c += bits as u64 * 256;
                }
                idx += num_4x4_w;
            }
            idy += num_4x4_h;
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
            // Record the decision for the emit pass (it only re-reads winning keys).
            self.mode_map.insert(mi_row, mi_col, bsize, (mi, predictor, self.last_trial_tx));
            self.store_mi(mi_row, mi_col, bwl, bhl, &mi);
            let bits_q8 = coef_q8 + self.inter_modeinfo_cost_q8(&mi, mi_row, mi_col, predictor);
            self.last_none_sse = sse;
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
        self.last_none_sse = sse;
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
        let _pc = prof::Scope::new(prof::S::PartCtx);
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
        let _s = prof::Scope::new(prof::S::SnapRestore);
        let rec = if self.snap_recon {
            std::array::from_fn(|p| {
                let (x0, y0, bw, bh) = self.block_px(mi_row, mi_col, bwl, bhl, p);
                let st = self.rec[p].stride;
                let mut v = take_u16(bw * bh);
                for r in 0..bh {
                    v.extend_from_slice(
                        &self.rec[p].buf[(y0 + r) * st + x0..(y0 + r) * st + x0 + bw],
                    );
                }
                v
            })
        } else {
            // Recon save/restore is redundant (every trial overwrites the block, winner is
            // re-reconstructed) — snapshot only the entropy context below.
            std::array::from_fn(|_| Vec::new())
        };
        let x_mis = (1usize << (bwl - 1)).min(self.mi_cols - mi_col);
        let y_mis = (1usize << (bhl - 1)).min(self.mi_rows - mi_row);
        let mut mi = take_mi(x_mis * y_mis);
        for y in 0..y_mis {
            let base = (mi_row + y) * self.mi_cols + mi_col;
            mi.extend_from_slice(&self.mi[base..base + x_mis]);
        }
        // Slice the above-ctx/seg snapshots to the block's span — cloning the
        // frame-width Vecs per snapshot was pure overhead (a trial only mutates
        // the columns the block covers).
        let above_ctx = std::array::from_fn(|pl| {
            let ss = self.rec[pl].ss_x;
            let c0 = (mi_col * 2) >> ss;
            let w = ((x_mis * 2) >> ss).max(1);
            let end = (c0 + w).min(self.above_ctx[pl].len());
            let mut v = take_u8(end.saturating_sub(c0));
            v.extend_from_slice(&self.above_ctx[pl][c0..end]);
            v
        });
        let seg_end = (mi_col + x_mis).min(self.above_seg.len());
        let mut above_seg = take_u8(seg_end.saturating_sub(mi_col));
        above_seg.extend_from_slice(&self.above_seg[mi_col..seg_end]);
        BlockSnap {
            rec,
            above_ctx,
            left_ctx: self.left_ctx,
            above_seg,
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
        let _sc = prof::Scope::new(prof::S::SnapRestore);
        if self.snap_recon {
            for p in 0..3 {
                let (x0, y0, bw, bh) = self.block_px(mi_row, mi_col, bwl, bhl, p);
                let st = self.rec[p].stride;
                for r in 0..bh {
                    let src = &s.rec[p][r * bw..r * bw + bw];
                    self.rec[p].buf[(y0 + r) * st + x0..(y0 + r) * st + x0 + bw]
                        .copy_from_slice(src);
                }
            }
        }
        for pl in 0..3 {
            let ss = self.rec[pl].ss_x;
            let c0 = (mi_col * 2) >> ss;
            let end = (c0 + s.above_ctx[pl].len()).min(self.above_ctx[pl].len());
            self.above_ctx[pl][c0..end].copy_from_slice(&s.above_ctx[pl][..end - c0]);
        }
        self.left_ctx = s.left_ctx;
        let sc0 = mi_col;
        let send = (sc0 + s.above_seg.len()).min(self.above_seg.len());
        self.above_seg[sc0..send].copy_from_slice(&s.above_seg[..send - sc0]);
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
        // Content-adaptive dispatch (Brick 3), decided once at the 64×64 SB root:
        // route this superblock through the variance partition (content-invariant
        // cost) when it is simple enough that RD search buys nothing, else keep the
        // full RD search. `var_part` forces the variance path for every SB (the
        // Brick-2 A/B). The variance tree, built here, is reused by whichever path
        // runs (the recursion reads `self.vt`).
        if bsize == BLOCK_64X64 && (self.var_part || self.dispatch) {
            self.build_vt(mi_row, mi_col);
            let use_var = self.var_part || {
                let v = self.vt.as_ref().unwrap().variance(3, 0, 0);
                // `dispatch_hi`: route the busy (RD-expensive) SBs to variance; else
                // route the flat SBs to variance and keep RD for the busy ones.
                if self.dispatch_hi {
                    v >= self.dispatch_thresh
                } else {
                    v < self.dispatch_thresh
                }
            };
            if use_var {
                return self.var_pick_partition(mi_row, mi_col, bsize, n4x4_l2);
            }
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

        // NONE — evaluated FIRST (the null arm). Its RD is the G1 gate feature for
        // skipping the expensive arms; end-state snapshotted, winner restored below.
        // Byte-identical to the old NONE-last order: every arm evaluates from `start`.
        let mut none_rd = f64::MAX;
        let mut none_snap = None;
        if full_fit {
            none_rd = self.part_flag_cost(&probs, PARTITION_NONE, has_rows, has_cols)
                + self.rd_block_none(mi_row, mi_col, bsize, n4x4_l2, n4x4_l2);
            none_snap = Some(self.snap_block(mi_row, mi_col, n4x4_l2, n4x4_l2));
            self.restore_block(mi_row, mi_col, n4x4_l2, n4x4_l2, &start);
        }
        let none_skip =
            self.mode_map.get(mi_row, mi_col, bsize).map_or(false, |(m, _, _)| m.skip);

        // G1 partition gate (discovered 2026-07-09, ceiling-swept on 1.6M nodes,
        // clip-level holdout): when NONE already fits this well, the expensive arms
        // (SPLIT recursion / sub-8x8 trials) win too rarely to pay for themselves.
        // Thresholds are none_rd/lambda per bsize, conservative percentiles that
        // kept >=99% of RD gain on the holdout. 64x64 is ungated (split wins 99%).
        // `VP9_NO_G1GATE` restores the exhaustive search (the oracle).
        let g1_skip = self.g1_gate
            && none_rd < f64::MAX
            && none_rd / self.lambda
                < self.g1_scale
                    * self.g1_area
                    * match bsize {
                        BLOCK_8X8 => 18.0, // bump to 33 at speed 0 was a weak trade (+0.16% BD)
                        6 => 64.0,  // 16x16
                        9 => 280.0, // 32x32
                        // 64×64: gated only at speed >= 3 (g1_64 = 0 disables).
                        _ => self.g1_64 / self.g1_scale.max(1e-9),
                    };

        // SPLIT — recurse into four quadrants (each leaves its own recon+context).
        let mut split_rd = f64::MAX;
        let mut split_snap = None;
        if can_split && !g1_skip {
            let subsize = subsize(PARTITION_SPLIT, bsize) as usize;
            let mut s = self.part_flag_cost(&probs, PARTITION_SPLIT, has_rows, has_cols);
            let quads = [
                (mi_row, mi_col),
                (mi_row, mi_col + hbs),
                (mi_row + hbs, mi_col),
                (mi_row + hbs, mi_col + hbs),
            ];
            let mut aborted = false;
            for (i, &(qr, qc)) in quads.iter().enumerate() {
                s += self.rd_pick_partition(qr, qc, subsize, n8x8_l2);
                // Early termination: once the running SPLIT total exceeds the
                // NONE incumbent the split provably loses. NOT byte-identical to
                // the exhaustive walk though — the skipped quadrants' mode_map
                // insertions are load-bearing for later cache lookups — so this
                // is a speed>=3 preset feature, BD-gated with the rest.
                if self.split_early && i < 3 && s > none_rd {
                    aborted = true;
                    break;
                }
            }
            if !aborted {
                split_rd = s;
                split_snap = Some(self.snap_block(mi_row, mi_col, n4x4_l2, n4x4_l2));
            }
            self.restore_block(mi_row, mi_col, n4x4_l2, n4x4_l2, &start);
        }

        // Sub-8×8 SPLIT at BLOCK_8X8: here PARTITION_SPLIT codes ONE sub-8×8 (BLOCK_4X4)
        // leaf with per-4×4 modes (not a recursion — `can_split` is false at 8×8).
        // Try all three: SPLIT (4×4), HORZ (8×4), VERT (4×8). Each codes ONE sub-8×8 leaf
        // of the corresponding subsize; the cheapest wins and its partition symbol is
        // recorded so the emit codes it.
        let mut sub_best: Option<(usize, f64, BlockSnap)> = None;
        if self.sub8x8 && self.is_inter && bsize == BLOCK_8X8 && full_fit && !g1_skip {
            for part in [PARTITION_SPLIT, PARTITION_HORZ, PARTITION_VERT] {
                let subsz = subsize(part, bsize) as usize; // BLOCK_4X4 / 8X4 / 4X8
                let (mi, coef, sse) =
                    self.decide_sub8x8(mi_row, mi_col, subsz, n4x4_l2, n4x4_l2, true);
                self.mode_map.insert(mi_row, mi_col, subsz, (mi, (0, 0), self.last_trial_tx));
                self.store_mi(mi_row, mi_col, n4x4_l2, n4x4_l2, &mi);
                let bits = coef + self.sub8x8_modeinfo_cost_q8(&mi, mi_row, mi_col);
                let rd = self.part_flag_cost(&probs, part, has_rows, has_cols)
                    + sse as f64
                    + self.lambda * (bits as f64 / 256.0);
                let snap = self.snap_block(mi_row, mi_col, n4x4_l2, n4x4_l2);
                self.restore_block(mi_row, mi_col, n4x4_l2, n4x4_l2, &start);
                if sub_best.as_ref().map_or(true, |b| rd < b.1) {
                    sub_best = Some((part, rd, snap));
                }
            }
        }
        let sub_rd = sub_best.as_ref().map_or(f64::MAX, |b| b.1);

        // G1 harvest tap (observe-only, env `VP9_G1_HARVEST`): the gate features +
        // both arms' outcomes, lambda-normalized offline. Zero cost when off.
        if self.g1_harvest {
            let sp = &self.src[0];
            let (bx, by, n) = (mi_col * 8, mi_row * 8, (1usize << n4x4_l2) * 4);
            let (mut sum, mut sq, mut cnt) = (0u64, 0u64, 0u64);
            let mut y = 0;
            while y < n && by + y < sp.h {
                let mut x = 0;
                while x < n && bx + x < sp.w {
                    let v = sp.buf[(by + y) * sp.stride + bx + x] as u64;
                    sum += v;
                    sq += v * v;
                    cnt += 1;
                    x += 2;
                }
                y += 2;
            }
            let var = if cnt > 0 { (sq as f64 - (sum as f64) * (sum as f64) / cnt as f64) / cnt as f64 } else { 0.0 };
            eprintln!(
                "G1 bsize={} q={} var={:.1} lambda={:.4} none={:.1} split={:.1} sub={:.1}",
                bsize, self.qindex, var, self.lambda, none_rd, split_rd,
                sub_best.as_ref().map_or(f64::MAX, |b| b.1)
            );
        }

        let best_split = split_rd.min(sub_rd);
        // Partition cascade: when NONE codes skip, bias the compare toward the large
        // skip block (its per-block header savings are real but small vs pred_sse).
        let none_gate = if none_skip { best_split * self.split_penalty } else { best_split };
        let (partition, cost) = if none_rd <= none_gate {
            // NONE ran first — reinstate its end-state.
            self.restore_block(mi_row, mi_col, n4x4_l2, n4x4_l2, &none_snap.unwrap());
            (PARTITION_NONE, none_rd)
        } else if sub_rd <= split_rd {
            // Sub-8×8 (SPLIT/HORZ/VERT) — restore the winning subsize's recon.
            let (part, rd, snap) = sub_best.unwrap();
            self.restore_block(mi_row, mi_col, n4x4_l2, n4x4_l2, &snap);
            (part, rd)
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

    /// Build the variance tree for the 64×64 superblock rooted at (`mi_row`,`mi_col`).
    /// Residual variance vs the zero-MV LAST reference on inter frames (the coding
    /// difficulty signal), source variance on key frames (no reference).
    fn build_vt(&mut self, mi_row: usize, mi_col: usize) {
        let _vt = prof::Scope::new(prof::S::VarTree);
        let (x0, y0) = (mi_col * 8, mi_row * 8);
        let (sw, sh, sstride) = (self.src[0].w, self.src[0].h, self.src[0].stride);
        let tree = {
            let sbuf = &self.src[0].buf;
            // refs[0] = LAST; aligned co-located (zero-MV) reference. Same cw×ch dims.
            let refp = self.refs[0].as_ref().map(|r| (&r[0].buf[..], r[0].stride));
            VarTree::build(sbuf, sstride, refp, x0, y0, sw, sh)
        };
        self.vt = Some(tree);
    }

    /// Brick 4 — per-frame adaptive dispatch threshold. A cheap O(pixels) pre-pass
    /// computes one root variance per 64×64 superblock (the "channel" measurement),
    /// then sets `dispatch_thresh` to the percentile matching `dispatch_q` (the
    /// target fraction of SBs routed to the variance partition). Because the cut is a
    /// percentile of *this frame's* distribution, the routing fraction — and thus the
    /// relative work split between the RD and variance algorithms — is content-
    /// invariant: the same `dispatch_q` yields the same split on akiyo and mobile,
    /// even though their absolute variance scales differ by ~50×. No-op in fixed-T
    /// mode (`VP9_DISPATCH_T`).
    fn set_dispatch_threshold(&mut self) {
        if !self.dispatch || self.dispatch_fixed_t {
            return;
        }
        let sb_rows = self.mi_rows.div_ceil(8);
        let sb_cols = self.mi_cols.div_ceil(8);
        let mut vars: Vec<i64> = Vec::with_capacity(sb_rows * sb_cols);
        for sbr in 0..sb_rows {
            for sbc in 0..sb_cols {
                self.build_vt(sbr * 8, sbc * 8);
                vars.push(self.vt.as_ref().unwrap().variance(3, 0, 0));
            }
        }
        self.vt = None;
        if vars.is_empty() {
            return;
        }
        vars.sort_unstable();
        let n = vars.len();
        // `dispatch_q` fraction goes to the variance partition. In the low-var
        // direction that fraction sits at the BOTTOM of the sorted variances (cut at
        // index q·n); in the high-var direction it sits at the TOP (cut at (1−q)·n).
        let frac = self.dispatch_q.clamp(0.0, 1.0);
        let idx = if self.dispatch_hi {
            ((n as f64) * (1.0 - frac)).round() as usize
        } else {
            ((n as f64) * frac).round() as usize
        };
        self.dispatch_thresh = if idx == 0 {
            i64::MIN
        } else if idx >= n {
            i64::MAX
        } else {
            vars[idx]
        };
    }

    /// Variance split threshold for a node at `level` (0=8×8 … 3=64×64). Mirrors
    /// libvpx's per-level scaling: smaller blocks tolerate more variance before
    /// splitting — a whole 64×64 is taken NONE only if very flat, a 16×16 readily.
    /// `base = var_thresh_mult · ac_dequant`; `VP9_VAR_THRESH` tunes the multiplier.
    fn vt_threshold(&self, level: usize) -> i64 {
        let base = self.var_thresh_mult * self.dq_y.1 as f64;
        let scale = match level {
            3 => 0.125, // 64×64 — split unless very flat
            2 => 0.5,   // 32×32
            _ => 8.0,   // 16×16 (level 0/8×8 never consults this)
        };
        (base * scale) as i64
    }

    /// Variance-driven partition for one node — the content-*invariant* alternative
    /// to `rd_pick_partition`. Chooses NONE vs SPLIT from the SB variance tree (no
    /// RD trial), then runs the SAME leaf machinery (`rd_block_none`) so the recon,
    /// contexts, and `part_map`/`mode_map` evolve exactly as the emit pass expects —
    /// making the stream decodable by construction, same guarantee as the RD path.
    /// Only NONE/SPLIT are produced (the emit path's vocabulary above 8×8); the
    /// finest granularity is an 8×8 NONE leaf (no sub-8×8 — that stays RD-only).
    fn var_pick_partition(
        &mut self,
        mi_row: usize,
        mi_col: usize,
        bsize: usize,
        n4x4_l2: usize,
    ) -> f64 {
        if mi_row >= self.mi_rows || mi_col >= self.mi_cols {
            return 0.0;
        }
        // (The variance tree is built once at the SB root by the dispatch block in
        // `rd_pick_partition` before this is entered; the recursion reuses it.)
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
        // NONE eligibility mirrors the RD path: `has_rows` and a full horizontal fit
        // (a `has_rows`-only block may overhang the bottom into the height padding;
        // horizontal overhang would need a padded stride, so it must split).
        let full_fit = has_rows && mi_col + num_8x8 <= self.mi_cols;
        let level = n4x4_l2 - 1;

        // Choose NONE vs SPLIT from the variance tree.
        let force_none = if !can_split {
            true // 8×8: finest granularity — always a NONE leaf
        } else if !full_fit {
            false // edge overhang can't code NONE → forced split (as the RD path)
        } else {
            let sb_mr = (mi_row / 8) * 8;
            let sb_mc = (mi_col / 8) * 8;
            let r = (mi_row - sb_mr) >> level;
            let c = (mi_col - sb_mc) >> level;
            let var = self.vt.as_ref().unwrap().variance(level, r, c);
            var < self.vt_threshold(level)
        };

        let (partition, cost) = if force_none {
            // Mark the leaf so `decide_inter` may take the non-RD fast path (LAST-ref
            // only + forced model-skip). Confined to this variance-routed leaf; the
            // RD partition path never sets `variance_leaf`.
            self.variance_leaf = true;
            let rd = self.part_flag_cost(&probs, PARTITION_NONE, has_rows, has_cols)
                + self.rd_block_none(mi_row, mi_col, bsize, n4x4_l2, n4x4_l2);
            self.variance_leaf = false;
            (PARTITION_NONE, rd)
        } else {
            let subsize = subsize(PARTITION_SPLIT, bsize) as usize;
            let mut s = self.part_flag_cost(&probs, PARTITION_SPLIT, has_rows, has_cols);
            for &(qr, qc) in &[
                (mi_row, mi_col),
                (mi_row, mi_col + hbs),
                (mi_row + hbs, mi_col),
                (mi_row + hbs, mi_col + hbs),
            ] {
                s += self.var_pick_partition(qr, qc, subsize, n8x8_l2);
            }
            (PARTITION_SPLIT, s)
        };
        self.part_map
            .insert((mi_row, mi_col, bsize), partition as u8);

        // Evolve the partition context exactly as the emit pass will.
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

    /// Joint compound refine: subpel-diamond `mv_search` (holding `mv_hold` fixed) to
    /// minimise the AVERAGED-prediction SAD `(search + hold + 1)>>1` vs source (a top-left-8×8
    /// proxy, like the subpel search). The HELD prediction is invariant across the diamond, so
    /// it's computed ONCE and only the search ref is re-predicted per candidate. ½- then ¼-pel.
    fn compound_refine(
        &self,
        mi_row: usize,
        mi_col: usize,
        mv_search: Mv,
        slot_search: usize,
        mv_hold: Mv,
        slot_hold: usize,
        edges: (i32, i32, i32, i32),
    ) -> Mv {
        let (base_x, base_y) = (mi_col * 8, mi_row * 8);
        let filt = self.active_filter as usize;
        // The held ref's prediction is constant across the search diamond — compute it once.
        let mut hold = [0u16; 64];
        {
            let q = clamp_mv_umv(mv_hold, 8, 8, 0, 0, edges);
            let rp = &self.refs[slot_hold].as_ref().unwrap()[0];
            let refp = RefPlane { buf: &rp.buf, stride: rp.stride, w: rp.w as i32, h: rp.h as i32 };
            predict_block(
                &refp, base_x as i32 + (q.1 >> 4), base_y as i32 + (q.0 >> 4),
                (q.1 & 15) as usize, (q.0 & 15) as usize, filt, &mut hold, 8, 8, 8, false, self.max_px,
            );
        }
        let src = &self.src[0];
        let s0 = base_y * src.stride + base_x;
        let rp = &self.refs[slot_search].as_ref().unwrap()[0];
        let refp = RefPlane { buf: &rp.buf, stride: rp.stride, w: rp.w as i32, h: rp.h as i32 };
        let score = |mv: Mv| -> i64 {
            let q = clamp_mv_umv(mv, 8, 8, 0, 0, edges);
            let mut buf = [0u16; 64];
            predict_block(
                &refp, base_x as i32 + (q.1 >> 4), base_y as i32 + (q.0 >> 4),
                (q.1 & 15) as usize, (q.0 & 15) as usize, filt, &mut buf, 8, 8, 8, false, self.max_px,
            );
            let mut sad = 0i64;
            for r in 0..8 {
                let sr = s0 + r * src.stride;
                for c in 0..8 {
                    let p = (buf[r * 8 + c] as i32 + hold[r * 8 + c] as i32 + 1) >> 1;
                    sad += (p - src.buf[sr + c] as i32).abs() as i64;
                }
            }
            sad
        };
        let mut best = mv_search;
        let mut best_sad = score(best);
        for &step in &[4i32, 2] {
            let (cr, cc) = best;
            for (dr, dc) in [(-step, 0), (step, 0), (0, -step), (0, step)] {
                let cand = (cr + dr, cc + dc);
                let sad = score(cand);
                if sad < best_sad {
                    best_sad = sad;
                    best = cand;
                }
            }
        }
        best
    }

    /// Full-block SSE of source vs the AVERAGED compound prediction `(ref0@mv0 + ref1@mv1
    /// + 1)>>1` under the current `active_filter` — the filter-search scorer for compound
    /// winners (mirrors the emit MC at encode_plane: ref0 avg=false, ref1 avg=true).
    fn pred_sse_compound(
        &self,
        mi_row: usize,
        mi_col: usize,
        mv0: Mv,
        slot0: usize,
        mv1: Mv,
        slot1: usize,
        edges: (i32, i32, i32, i32),
        bwl: usize,
        bhl: usize,
    ) -> i64 {
        let (base_x, base_y) = (mi_col * 8, mi_row * 8);
        let (w, h) = ((1usize << bwl) * 4, (1usize << bhl) * 4);
        let filt = self.active_filter as usize;
        // Reused rather than a `[0u16; 64*64]` local: that is an 8 KB zero-init on
        // every call, and this runs per compound candidate whenever `-lag` is
        // active. Same bug class as `TxScratch`. Safe because the ref-0 pass
        // (`avg = false`) writes all of `[..w*h]` before the ref-1 pass blends
        // into it and before the SSE loop reads it.
        thread_local! {
            static PSC_BUF: std::cell::RefCell<Vec<u16>> =
                std::cell::RefCell::new(vec![0u16; 64 * 64]);
        }
        PSC_BUF.with(|b| {
            let mut buf = b.borrow_mut();
            if self.tx_memset {
                buf.fill(0); // VP9_TX_MEMSET=1 — reproduce the old per-call cost
            }
            for (i, (mv, slot)) in [(mv0, slot0), (mv1, slot1)].iter().enumerate() {
                let q = clamp_mv_umv(*mv, w as i32, h as i32, 0, 0, edges);
                let rp = &self.refs[*slot].as_ref().unwrap()[0];
                let refp =
                    RefPlane { buf: &rp.buf, stride: rp.stride, w: rp.w as i32, h: rp.h as i32 };
                predict_block(
                    &refp, base_x as i32 + (q.1 >> 4), base_y as i32 + (q.0 >> 4),
                    (q.1 & 15) as usize, (q.0 & 15) as usize, filt, &mut buf, w, w, h, i == 1,
                    self.max_px,
                );
            }
            let src = &self.src[0];
            let mut sse = 0i64;
            for r in 0..h {
                let s_row = &src.buf[(base_y + r) * src.stride + base_x..][..w];
                let b_row = &buf[r * w..][..w];
                for c in 0..w {
                    let d = b_row[c] as i64 - s_row[c] as i64;
                    sse += d * d;
                }
            }
            sse
        })
    }

    /// AQ segment of the 64×64 SB covering `(mi_row, mi_col)` (0 when AQ is off).
    fn sb_seg(&self, mi_row: usize, mi_col: usize) -> u8 {
        if !self.aq_active || self.aq_seg.is_empty() {
            return 0;
        }
        let idx = (mi_row / 8) * self.aq_ncols + (mi_col / 8);
        self.aq_seg.get(idx).copied().unwrap_or(0)
    }

    /// Set the per-SB dequant + RD-λ from the AQ segment (called at each SB root in the
    /// decision AND emit passes — deterministic segment ⇒ they can't disagree).
    fn set_sb_aq(&mut self, mi_row: usize, mi_col: usize) {
        if !self.aq_active {
            return;
        }
        let s = self.sb_seg(mi_row, mi_col) as usize;
        self.dq_y = self.aq_dq_y[s];
        self.dq_uv = self.aq_dq_uv[s];
        self.lambda = self.aq_lambda[s];
    }

    /// Fill `part_map` by running the recursive partition RD over every superblock,
    /// then leave `rec`/context ready to be reset for the emit pass.
    fn run_partition_decision(&mut self) {
        self.part_map.clear();
        self.mode_map.clear();
        let ntiles = 1usize << self.tile_cols_log2;
        if ntiles > 1 {
            // Tile columns are decision-independent (MC reads the reference frames,
            // intra/contexts/MV-refs are tile-bounded), so the RD pass — 97% of
            // encode — runs one clone per tile in parallel. Only the decision maps
            // survive; the sequential emit replays them into the real state.
            let decided = std::thread::scope(|sc| {
                let handles: Vec<_> = (0..ntiles)
                    .map(|t| {
                        let mut worker = self.clone();
                        sc.spawn(move || {
                            worker.decide_tile(t);
                            (worker.part_map, worker.mode_map)
                        })
                    })
                    .collect();
                handles
                    .into_iter()
                    .map(|h| h.join().expect("tile decision thread"))
                    .collect::<Vec<_>>()
            });
            for (pm, mm) in decided {
                self.part_map.extend(pm);
                self.mode_map.merge(mm);
            }
            return;
        }
        self.decide_tile(0);
    }

    /// The RD decision pass over one tile column (`rd_pick_partition` per SB).
    fn decide_tile(&mut self, t: usize) {
        let _tot = prof::Scope::new(prof::S::Total); // VP9_PROF2 % denominator (per tile/thread)
        {
            let ts = tile_offset_enc(t, self.mi_cols, self.tile_cols_log2);
            let te = tile_offset_enc(t + 1, self.mi_cols, self.tile_cols_log2);
            self.tile_start = ts;
            self.tile_end = te;
            for (p, c) in self.above_ctx.iter_mut().enumerate() {
                let ss = (p > 0) as usize;
                c[(ts * 2) >> ss..(te * 2) >> ss].iter_mut().for_each(|v| *v = 0);
            }
            self.above_seg[ts..te].iter_mut().for_each(|v| *v = 0);
            let mut mi_row = 0;
            while mi_row < self.mi_rows {
                self.left_seg = [0; 8];
                self.left_ctx = [[0; 16]; 3];
                let mut mi_col = ts;
                while mi_col < te {
                    self.set_sb_aq(mi_row, mi_col);
                    self.rd_pick_partition(mi_row, mi_col, BLOCK_64X64, 4);
                    mi_col += 8;
                }
                mi_row += 8;
            }
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
        // Sub-8×8 leaf (bsize<8×8, reached via SPLIT at BLOCK_8X8) decides per-4×4 modes;
        // an 8×8+ block decides a single mode. When the RD pass already decided this
        // leaf, reuse its decision instead of re-running the whole mode search — the
        // commit below redoes the reconstruction either way, so only the (identical)
        // search is skipped. `active_ref` must be re-locked for the commit MC.
        let cached = self.mode_map.get(mi_row, mi_col, bsize);
        let (mi, predictor) = if let Some((m, p, trial_tx)) = cached {
            self.active_ref = match m.ref_frame[0] {
                GOLDEN_FRAME => 1,
                ALTREF_FRAME => 2,
                _ => 0,
            };
            // The block's chosen filter drives the commit MC on a switchable frame; on
            // a fixed frame (incl. a frame the gate flipped to EIGHTTAP) use the frame
            // filter, so mode_map's per-block filters are ignored and recon stays in sync.
            if m.is_inter {
                self.active_filter = if self.interp_filter == 4 {
                    m.interp_filter
                } else {
                    self.interp_filter as u8
                };
            }
            if m.skip {
                // A skip block emits no tokens: the commit relies on the recon +
                // zeroed entropy contexts the decide-trial left behind. Replay
                // that trial (at its ORIGINAL tx size — skip syntax substitutes
                // max_tx into `m.tx_size`) to reproduce those side effects.
                let mut trial = m;
                trial.skip = false;
                trial.tx_size = trial_tx;
                self.pending_eob = 0;
                self.skip_trial = true;
                // force_skip reproduces the empty-block recon (MC prediction, zeroed
                // contexts) for BOTH a naturally-empty skip (eob already 0, no-op) and
                // an RD-forced skip (drops the residual the trial would otherwise add).
                self.force_skip = true;
                for plane in 0..3 {
                    self.encode_plane(
                        None,
                        &trial,
                        plane,
                        mi_row,
                        mi_col,
                        trial.sb_type as usize,
                        bwl,
                        bhl,
                    );
                }
                self.force_skip = false;
                self.skip_trial = false;
            }
            (m, p)
        } else if bsize < BLOCK_8X8 {
            let (m, _, _) = self.decide_sub8x8(mi_row, mi_col, bsize, bwl, bhl, false);
            (m, (0i32, 0i32))
        } else {
            let (m, p, _, _) = self.decide_inter(mi_row, mi_col, bsize, bwl, bhl, false);
            (m, p)
        };
        // Lock the block's filter for the commit MC: the block's own filter on a
        // switchable frame, else the frame filter (also covers a gate-flipped frame).
        if mi.is_inter {
            self.active_filter = if self.interp_filter == 4 {
                mi.interp_filter
            } else {
                self.interp_filter as u8
            };
        }
        let above = self.above_mi(mi_row, mi_col);
        let left = self.left_mi(mi_row, mi_col);
        // Use the ACTUAL coded size (sub-8×8 sets sb_type<8×8): its max tx is 4×4, so
        // tx_size is not coded, exactly as the decoder gates it.
        let max_tx = MAX_TXSIZE[mi.sb_type as usize] as usize;

        // AQ: segment_id is the first per-block syntax (inter frame — tree only, no temporal
        // prediction). Then skip, is_inter, tx_size (not coded when skipped).
        if self.aq_active {
            write_segment_id(enc, self.sb_seg(mi_row, mi_col), &AQ_TREE_PROBS);
        }
        let sctx = skip_context(above.as_ref(), left.as_ref());
        write_skip(enc, mi.skip, self.fc.skip_probs[sctx]);
        let ictx = intra_inter_context(above.as_ref(), left.as_ref());
        write_is_inter(enc, mi.is_inter, self.fc.intra_inter_prob[ictx]);
        if self.use_tx_search && max_tx >= 1 && !mi.skip {
            let ctx = tx_size_context(&mi, above.as_ref(), left.as_ref());
            self.write_tx_size(enc, mi.tx_size, ctx, max_tx);
        }
        if mi.is_inter {
            // REF-SELECTION HARVEST (`VP9_REF_HIST=1`, observe-only). Counts which
            // reference each EMITTED inter block actually chose, so "the ALT-REF is
            // coded but never used" can be measured instead of assumed. Indices:
            // 0=LAST 1=GOLDEN 2=ALTREF 3=compound.
            if ref_hist_on() {
                let i = if mi.has_second_ref() {
                    3
                } else {
                    (mi.ref_frame[0] - LAST_FRAME).clamp(0, 2) as usize
                };
                REF_HIST[i].fetch_add(1, std::sync::atomic::Ordering::Relaxed);
            }
            // Reference. On a SELECT frame (compound), a comp_inter bit picks
            // single-vs-compound; compound then codes a comp_ref bit (which var ref
            // pairs with the fixed GOLDEN), else the usual single_ref pair. Mirrors
            // the decoder's `read_ref_frames`.
            let write_single = |enc: &mut BoolEncoder, fc: &FrameContext, rf: i8| {
                let ctx0 = single_ref_p1(above.as_ref(), left.as_ref());
                let ctx1 = single_ref_p2(above.as_ref(), left.as_ref());
                write_single_ref(enc, rf, fc.single_ref_prob[ctx0][0], fc.single_ref_prob[ctx1][1]);
            };
            if self.fc.reference_mode == 2 {
                let rmctx = reference_mode_context(
                    above.as_ref(), left.as_ref(), self.sign_bias, self.fc.comp_fixed_ref,
                );
                let is_comp = mi.has_second_ref();
                write_comp_inter(enc, is_comp, self.fc.comp_inter_prob[rmctx]);
                if is_comp {
                    let crctx = comp_ref_context(above.as_ref(), left.as_ref(), self.sign_bias, &self.fc);
                    // The var ref sits opposite the fixed ref's slot; its bit selects
                    // which comp_var_ref element it is.
                    let idx = self.sign_bias[self.fc.comp_fixed_ref] as usize;
                    let var_ref = mi.ref_frame[1 - idx];
                    let var_bit = (var_ref == self.fc.comp_var_ref[1] as i8) as u32;
                    write_comp_ref(enc, var_bit, self.fc.comp_ref_prob[crctx]);
                } else {
                    write_single(enc, &self.fc, mi.ref_frame[0]);
                }
            } else {
                write_single(enc, &self.fc, mi.ref_frame[0]);
            }
            // interp_filter is coded per inter block on a switchable frame, right after
            // the reference (and the 8×8+ block mode) and BEFORE any MV — mirroring the
            // decoder's read order (ref → inter_mode → interp_filter → MV).
            let write_filter = |enc: &mut BoolEncoder, fc: &FrameContext, mi: &ModeInfo| {
                let ctx = switchable_interp_context(above.as_ref(), left.as_ref());
                write_interp_filter(enc, mi.interp_filter, &fc.switchable_interp_prob[ctx]);
            };
            if (mi.sb_type as usize) < BLOCK_8X8 {
                // Sub-8×8: per-4×4 mode + MV (NEWMV coded relative to the shared
                // find_mv_refs(NEWMV) predictor). All sizes/contexts use the SUBSIZE
                // (mi.sb_type), exactly as the decoder does.
                if self.interp_filter == 4 {
                    write_filter(enc, &self.fc, &mi);
                }
                let sub = mi.sb_type as usize;
                let mctx = get_mode_context(
                    &self.mi, self.mi_cols, self.mi_rows, self.tile_start, self.tile_end, mi_row, mi_col, sub,
                );
                let edges = self.block_edges(mi_row, mi_col, sub);
                let (cand, _) = find_mv_refs(
                    &self.mi, self.mi_cols, self.mi_rows, self.tile_start, self.tile_end, mi_row, mi_col, sub,
                    mi.ref_frame[0], &self.sign_bias, NEWMV, -1, edges, self.prev_mv(mi_row, mi_col),
                );
                let pred = lower_mv_precision(cand[0], self.hp_mv);
                let num_4x4_w = 1usize << B_WIDTH_LOG2[sub];
                let num_4x4_h = 1usize << B_HEIGHT_LOG2[sub];
                let mut idy = 0;
                while idy < 2 {
                    let mut idx = 0;
                    while idx < 2 {
                        let j = idy * 2 + idx;
                        write_inter_mode(enc, mi.bmi[j], &self.fc.inter_mode_probs[mctx]);
                        if mi.bmi[j] == NEWMV {
                            let mut counts = NmvCounts::default();
                            encode_mv(enc, mi.bmi_mv[j][0], pred, &self.fc.nmvc, self.hp_mv, &mut counts);
                        }
                        idx += num_4x4_w;
                    }
                    idy += num_4x4_h;
                }
            } else {
                let mctx = get_mode_context(
                    &self.mi, self.mi_cols, self.mi_rows, self.tile_start, self.tile_end, mi_row, mi_col, bsize,
                );
                write_inter_mode(enc, mi.mode, &self.fc.inter_mode_probs[mctx]);
                if self.interp_filter == 4 {
                    write_filter(enc, &self.fc, &mi);
                }
                if mi.mode == NEWMV {
                    let mut counts = NmvCounts::default();
                    encode_mv(enc, mi.mv[0], predictor, &self.fc.nmvc, self.hp_mv, &mut counts);
                    // Compound NEWMV codes a SECOND MV for ref[1] (GOLDEN), against its
                    // own find_mv_refs predictor — exactly the decoder's per-ref assign_mv.
                    if mi.has_second_ref() {
                        let edges = self.block_edges(mi_row, mi_col, bsize);
                        let (cand1, _) = find_mv_refs(
                            &self.mi, self.mi_cols, self.mi_rows, self.tile_start, self.tile_end,
                            mi_row, mi_col, bsize, mi.ref_frame[1], &self.sign_bias, NEWMV, -1,
                            edges, self.prev_mv(mi_row, mi_col),
                        );
                        let pred1 = lower_mv_precision(cand1[0], self.hp_mv);
                        encode_mv(enc, mi.mv[1], pred1, &self.fc.nmvc, self.hp_mv, &mut counts);
                    }
                }
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
                // Coded size = sb_type (subsize for sub-8×8) so the residual (4×4 tx) and
                // MC dispatch match the trial in decide_sub8x8.
                self.encode_plane(Some(enc), &mi, plane, mi_row, mi_col, mi.sb_type as usize, bwl, bhl);
            }
        }
    }

    /// Sub-8×8 per-4×4 MV predictor — the exact inverse of the decoder's
    /// `append_sub8x8_mvs` (decode.rs). For sub-block `block` (0..4) it derives the
    /// NEAREST/NEAR candidate from the already-decided earlier sub-blocks' `bmi_mv`
    /// and, when needed, a `find_mv_refs` scan at that sub-block index. Encoder uses
    /// the single-tile range `0..mi_cols` and `None` temporal predictor (mirrors the
    /// error-resilient decode path), so the candidate matches the decoder bit-exactly.
    #[allow(clippy::too_many_arguments, dead_code)] // wired in sub-8×8 Part B
    fn enc_sub8x8_mv(
        &self,
        mi: &ModeInfo,
        b_mode: u8,
        block: usize,
        r: usize,
        mi_row: usize,
        mi_col: usize,
        bsize: usize,
        edges: (i32, i32, i32, i32),
    ) -> Mv {
        let frame = mi.ref_frame[r];
        let find = |blk: i32| {
            find_mv_refs(
                &self.mi,
                self.mi_cols,
                self.mi_rows,
                self.tile_start,
                self.tile_end,
                mi_row,
                mi_col,
                bsize,
                frame,
                &self.sign_bias,
                b_mode,
                blk,
                edges,
                self.prev_mv(mi_row, mi_col),
            )
        };
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
        let _sdl = prof::Scope::new(prof::S::DecideLeaf);
        // --- motion + mode search over every available reference (LAST/GOLDEN/ALTREF).
        // Each ref gets its own `find_mv_refs` predictor + motion search, then RD over
        // ZEROMV/NEWMV; the cheapest (ref, mode) across all refs wins (J = SSE+λ·bits). ---
        let edges = self.block_edges(mi_row, mi_col, bsize);
        let snap = self.snap_y(mi_row, mi_col, bwl, bhl);
        // On a switchable frame the mode/MV search runs at EIGHTTAP; the per-block
        // filter is refined after the winner (below). Reset here so a previous block's
        // chosen filter doesn't leak into this block's search.
        if self.interp_filter == 4 {
            self.active_filter = 0;
        }
        let ifilt = self.active_filter as u8; // 0 (EIGHTTAP) while searching a switchable frame
        let base_tx = self.base_tx(bsize);
        let mk_inter = |rf: i8, mode: u8, mv: Mv| ModeInfo {
            sb_type: bsize as u8,
            skip: false,
            tx_size: base_tx,
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
        // Accurate per-mode signalling cost (bits): the real inter-mode-tree cost at this
        // block's mode context, replacing the old rough 4.0/16.0 constants so the mode
        // search's RD matches what will actually be coded.
        let mctx = get_mode_context(
            &self.mi, self.mi_cols, self.mi_rows, self.tile_start, self.tile_end, mi_row, mi_col, bsize,
        );
        let mode_bits = |m: u8| -> f64 {
            tree_bit_cost(
                &INTER_MODE_TREE,
                &self.fc.inter_mode_probs[mctx],
                (m - NEARESTMV) as i32,
            ) as f64
                / 256.0
        };
        let (c_zero, c_nearest, c_near, c_new) = (
            mode_bits(ZEROMV),
            mode_bits(NEARESTMV),
            mode_bits(NEARMV),
            mode_bits(NEWMV),
        );
        // Brick 2b — collect every (ref × mode) candidate with its cheap skip-RD
        // estimate J_skip = pred_SSE + λ·bits, then full-RD (transform) only the
        // top `shortlist_k`. Candidate = (J_skip, slot, rf, mode, mv, predictor, extra_bits).
        let mut topk = [f64::INFINITY; 4];
        let mut cands: Vec<(f64, usize, i8, u8, Mv, Mv, f64)> = Vec::with_capacity(12);
        let mut g3_last_j = f64::INFINITY;
        // Per-slot search results (compound reuses these): slot_pred = NEARESTMV MV,
        // slot_mv = NEWMV; per-ref, indexed by slot.
        let mut slot_mv = [(0i32, 0i32); 3];
        let mut slot_pred = [(0i32, 0i32); 3];
        let mut slot_j = [f64::INFINITY; 3]; // best single-ref J per slot (compound var pick)
        let mut slot_searched = [false; 3];
        // Non-RD leaf fast path (variance-routed leaves only): LAST-ref only — skip the
        // GOLDEN/ALTREF motion search + candidate transforms entirely. The single
        // biggest floor lever, since each extra ref is a full search + shortlist.
        let fast = self.nonrd_leaf && self.variance_leaf;
        for (slot, rf) in [(0usize, LAST_FRAME), (1, GOLDEN_FRAME), (2, ALTREF_FRAME)] {
            if fast && slot > 0 {
                break;
            }
            if slot == 1 {
                // G3 gate: skip GOLDEN/ALTREF when LAST already fits well. The
                // shortlist defers full-RD, so gate on LAST's best skip-RD estimate.
                if self.g3_gate && g3_last_j / self.lambda < 32.0 {
                    break;
                }
            }
            if self.refs[slot].is_none() {
                continue;
            }
            self.active_ref = slot;
            let (cand, _) = {
                let _mvr = prof::Scope::new(prof::S::MvRefs);
                find_mv_refs(
                    &self.mi, self.mi_cols, self.mi_rows, self.tile_start, self.tile_end,
                    mi_row, mi_col, bsize, rf, &self.sign_bias, NEWMV, -1, edges,
                    self.prev_mv(mi_row, mi_col),
                )
            };
            let predictor = lower_mv_precision(cand[0], self.hp_mv);
            let best_mv = self.search_mv(mi_row, mi_col, predictor, bwl, bhl);
            slot_mv[slot] = best_mv;
            slot_pred[slot] = predictor;
            slot_searched[slot] = true;
            // Rough ref-signalling cost so LAST (one bool) is preferred over GOLDEN/ALTREF.
            let ref_bits = if rf == LAST_FRAME { 1.0 } else { 2.0 };
            let mut ref_min = f64::INFINITY;
            let mut add = |cands: &mut Vec<_>,
                           ref_min: &mut f64,
                           topk: &mut [f64; 4],
                           mode,
                           mv,
                           sse: i64,
                           extra: f64| {
                // Rank by the Laplacian model's estimated CODED (dist, residual-rate) rather
                // than raw pred-SSE — pred-SSE omits the residual bits that vary per candidate,
                // which is exactly why a pred-SSE shortlist needs a large K. Model ranking is
                // meant to shrink K (fewer full reconstructs) at neutral BD.
                let js = if self.model_rank {
                    let (r, d) = varrd::model_rd(
                        sse.max(0) as u64,
                        (bwl + bhl + 4) as u32,
                        self.dq_y.1 as i64,
                    );
                    d + self.lambda * (r + extra)
                } else {
                    sse as f64 + self.lambda * extra
                };
                *ref_min = ref_min.min(js);
                // Maintain the K smallest J's seen so far (K ≤ 4 slots).
                let mut v = js;
                for t in topk.iter_mut() {
                    if v < *t {
                        std::mem::swap(&mut v, t);
                    }
                }
                cands.push((js, slot, rf, mode, mv, predictor, extra));
            };
            // Abort bound for the next candidate: it must beat the current
            // kth-best J to make the shortlist (∞ until K have been collected,
            // or when the shortlist is off — the full-RD-all oracle).
            let kq = if self.mode_shortlist { self.shortlist_k.clamp(1, 4) } else { usize::MAX };
            let bound_for = |topk: &[f64; 4], extra: f64| -> f64 {
                if kq <= 4 && topk[kq - 1].is_finite() {
                    topk[kq - 1] - self.lambda * extra
                } else {
                    f64::INFINITY
                }
            };
            // ZEROMV (always a candidate).
            let e = c_zero + ref_bits;
            let sse = self.pred_sse(mi_row, mi_col, (0, 0), edges, bwl, bhl, bound_for(&topk, e));
            add(&mut cands, &mut ref_min, &mut topk, ZEROMV, (0, 0), sse, e);
            if best_mv != (0, 0) {
                let dr = (best_mv.0 - predictor.0).unsigned_abs();
                let dc = (best_mv.1 - predictor.1).unsigned_abs();
                let mvb =
                    (10 + 2 * ((32 - dr.leading_zeros()) + (32 - dc.leading_zeros()))) as f64;
                let e = c_new + mvb + ref_bits;
                let sse =
                    self.pred_sse(mi_row, mi_col, best_mv, edges, bwl, bhl, bound_for(&topk, e));
                add(&mut cands, &mut ref_min, &mut topk, NEWMV, best_mv, sse, e);
            }
            // NEARESTMV uses the nearest candidate; skip when it degenerates to (0,0).
            if predictor != (0, 0) {
                let e = c_nearest + ref_bits;
                let sse =
                    self.pred_sse(mi_row, mi_col, predictor, edges, bwl, bhl, bound_for(&topk, e));
                add(&mut cands, &mut ref_min, &mut topk, NEARESTMV, predictor, sse, e);
            }
            // NEARMV uses the DISTINCT second candidate (re-scan to match the decoder).
            let (cand_near, _) = find_mv_refs(
                &self.mi, self.mi_cols, self.mi_rows, self.tile_start, self.tile_end,
                mi_row, mi_col, bsize, rf, &self.sign_bias, NEARMV, -1, edges,
                self.prev_mv(mi_row, mi_col),
            );
            let mv_near = lower_mv_precision(cand_near[1], self.hp_mv);
            if mv_near != (0, 0) && mv_near != predictor {
                let e = c_near + ref_bits;
                let sse =
                    self.pred_sse(mi_row, mi_col, mv_near, edges, bwl, bhl, bound_for(&topk, e));
                add(&mut cands, &mut ref_min, &mut topk, NEARMV, mv_near, sse, e);
            }
            slot_j[slot] = ref_min; // per-ref best skip-RD estimate (compound var pick)
            if slot == 0 {
                g3_last_j = ref_min; // LAST's best skip-RD estimate → the G3 feature
            }
        }
        // Full-RD only the top-K candidates by skip-RD estimate (all when the
        // shortlist is off — the A/B oracle).
        cands.sort_by(|a, b| a.0.partial_cmp(&b.0).unwrap_or(std::cmp::Ordering::Equal));
        // (A content-adaptive shortlist BUMP — more full-RD candidates on high-residual
        // blocks — was tested and PRUNED: a wash (akiyo −0.38% but foreman/mobile +0.06..0.10%).
        // More candidates CAN lose because the locally-optimal mode it finds hurts downstream
        // entropy adaptation — the mode λ is already well-calibrated, like the sigrate result.)
        let k = if self.mode_shortlist {
            self.shortlist_k.max(1)
        } else {
            cands.len()
        };
        for &(js, slot, rf, mode, mv, predictor, extra) in cands.iter().take(k) {
            // Adaptive mode-skip (libvpx adaptive_rd_thresh, speed >= 3): the
            // list is sorted by the J_skip estimate; once an estimate exceeds
            // the best FULL J by the factor, stop trialling entirely.
            if self.mode_thresh_mult > 0.0
                && best_inter.0.is_finite()
                && js > best_inter.0 * self.mode_thresh_mult
            {
                break;
            }
            self.active_ref = slot;
            let mi_c = mk_inter(rf, mode, mv);
            // chroma-RD costs each candidate on luma+chroma. A top-2-only variant
            // (luma-rank the candidates, chroma-refine the top 2) was ~1.05× vs 1.18×
            // but LOST the win (+0.21% vs −0.30%): the chroma tiebreak needs candidates
            // the luma-only break prunes. Full yuv per candidate is the keeper.
            let j = if self.chroma_rd {
                self.rd_cost_yuv(&mi_c, mi_row, mi_col, bsize, bwl, bhl, extra, best_inter.0)
            } else {
                self.rd_cost_y(
                    &mi_c, mi_row, mi_col, bsize, bwl, bhl, &snap, extra, best_inter.0,
                )
            };
            if j < best_inter.0 {
                best_inter = (j, slot, rf, mode, mv, predictor);
            }
        }

        // --- Compound (bi-directional): LAST+GOLDEN averaged prediction (Brick 2). ---
        // Reuses the per-slot NEWMV searches; full-RD-compared against the single-ref
        // winner. ZEROMV compound codes no MV bits; NEWMV compound codes two.
        let mut compound_mi: Option<ModeInfo> = None;
        let mut compound_j = best_inter.0;
        // Content-adaptive gate: skip the compound trials on blocks the single ref already
        // predicts well (best_inter.0 ≤ compound_gate·λ) — bi-pred only pays where the
        // residual is high. λ-normalized ⇒ one threshold across QPs.
        let gate_ok = self.compound_gate <= 0.0
            || best_inter.0 > self.compound_gate * self.lambda;
        if self.compound && !self.compound_force && self.fc.reference_mode == 2 && gate_ok {
            // Fixed compound ref (ALTREF for bi-pred, GOLDEN for the LAST+GOLDEN fallback);
            // it pairs with each searched var ref. Placement: sign_bias[fixed]=1 ⇒ the
            // fixed ref lands in slot 1, the var in slot 0 (mv[0]=var, mv[1]=fixed).
            let fixed_rf = self.fc.comp_fixed_ref as i8;
            let var_refs = self.fc.comp_var_ref;
            let fixed_slot = (fixed_rf as usize).wrapping_sub(1);
            let mvcost = |mv: Mv, pred: Mv| -> f64 {
                let dr = (mv.0 - pred.0).unsigned_abs();
                let dc = (mv.1 - pred.1).unsigned_abs();
                (10 + 2 * ((32 - dr.leading_zeros()) + (32 - dc.leading_zeros()))) as f64
            };
            // Content-adaptive var pick: pair the fixed ref with only the var ref that
            // predicts BEST on its own (lower single-ref J). The other var almost never
            // wins the averaged prediction, so this halves the compound trials at ~0 BD.
            // `VP9_COMPOUND_ALLVAR` restores the exhaustive both-vars A/B oracle.
            let all_var = std::env::var("VP9_COMPOUND_ALLVAR").is_ok();
            let best_var = if all_var {
                usize::MAX // sentinel: don't filter
            } else {
                var_refs
                    .iter()
                    .copied()
                    .filter(|&vr| {
                        let s = vr.wrapping_sub(1);
                        s < 3 && slot_searched[s]
                    })
                    .min_by(|&a, &b| {
                        slot_j[a.wrapping_sub(1)]
                            .partial_cmp(&slot_j[b.wrapping_sub(1)])
                            .unwrap_or(std::cmp::Ordering::Equal)
                    })
                    .unwrap_or(usize::MAX)
            };
            if fixed_slot < 3 && slot_searched[fixed_slot] {
                let (fixed_mv, fixed_pred) = (slot_mv[fixed_slot], slot_pred[fixed_slot]);
                for &vr in &var_refs {
                    let var_slot = vr.wrapping_sub(1);
                    if var_slot >= 3 || !slot_searched[var_slot] {
                        continue;
                    }
                    if best_var != usize::MAX && vr != best_var {
                        continue; // only the content-chosen best var ref
                    }
                    let (var_rf, var_mv, var_pred) =
                        (vr as i8, slot_mv[var_slot], slot_pred[var_slot]);
                    let new_mvbits = mvcost(var_mv, var_pred) + mvcost(fixed_mv, fixed_pred);
                    let mut comp_cands: Vec<(u8, Mv, Mv, f64, f64)> = vec![
                        (ZEROMV, (0, 0), (0, 0), 0.0, c_zero),
                        (NEWMV, var_mv, fixed_mv, new_mvbits, c_new),
                    ];
                    // Joint compound MV refinement: refine each MV against the AVERAGED
                    // prediction (the single-ref bests aren't jointly optimal), and add the
                    // refined pair as an EXTRA NEWMV candidate — the full RD below keeps it
                    // only when it truly wins (the cheap SAD search proposes, the RD disposes).
                    if self.compound_joint {
                        let jv = self.compound_refine(mi_row, mi_col, var_mv, var_slot, fixed_mv, fixed_slot, edges);
                        let jf = self.compound_refine(mi_row, mi_col, fixed_mv, fixed_slot, jv, var_slot, edges);
                        if (jv, jf) != (var_mv, fixed_mv) {
                            let jb = mvcost(jv, var_pred) + mvcost(jf, fixed_pred);
                            comp_cands.push((NEWMV, jv, jf, jb, c_new));
                        }
                    }
                    // NEAREST/NEAR compound (opt-in): NO MV bits — the decoder DERIVES both
                    // MVs per ref as `find_mv_refs(ref, mode)[idx]` (idx=1 for NEARMV else 0),
                    // lowered. Reproduce EXACTLY (the earlier slot_pred used mode=NEWMV and
                    // desynced). Computed before the &mut rd trials so the &self.mi borrow ends.
                    if self.compound_near {
                        let hp = self.hp_mv;
                        let derive = |rf: i8, mode: u8| -> Mv {
                            let (tmp, _) = find_mv_refs(
                                &self.mi, self.mi_cols, self.mi_rows, self.tile_start,
                                self.tile_end, mi_row, mi_col, bsize, rf, &self.sign_bias, mode,
                                -1, edges, self.prev_mv(mi_row, mi_col),
                            );
                            lower_mv_precision(tmp[if mode == NEARMV { 1 } else { 0 }], hp)
                        };
                        let pen = self.compound_near_penalty;
                        comp_cands.push((NEARESTMV, derive(var_rf, NEARESTMV), derive(fixed_rf, NEARESTMV), pen, c_nearest));
                        comp_cands.push((NEARMV, derive(var_rf, NEARMV), derive(fixed_rf, NEARMV), pen, c_near));
                    }
                    for &(cmode, mv0, mv1, mvbits, mbits) in comp_cands.iter() {
                        let mut cmi = mk_inter(var_rf, cmode, mv0);
                        cmi.ref_frame = [var_rf, fixed_rf]; // [var, fixed]
                        cmi.mv = [mv0, mv1];
                        let extra = 2.0 + mbits + mvbits; // comp_inter + comp_ref ≈ 2
                        let j = if self.chroma_rd {
                            self.rd_cost_yuv(&cmi, mi_row, mi_col, bsize, bwl, bhl, extra, compound_j)
                        } else {
                            self.rd_cost_y(
                                &cmi, mi_row, mi_col, bsize, bwl, bhl, &snap, extra, compound_j,
                            )
                        };
                        if j < compound_j {
                            compound_j = j;
                            compound_mi = Some(cmi);
                        }
                    }
                }
            }
        }

        if self.g1_harvest && g3_last_j < f64::INFINITY {
            eprintln!(
                "G3 last_j={:.1} lambda={:.4} winner_slot={}",
                g3_last_j, self.lambda, best_inter.1
            );
        }
        // --- intra alternative (reference-independent) ---
        let mut intra_mi = ModeInfo {
            sb_type: bsize as u8,
            skip: false,
            tx_size: self.base_tx(bsize),
            is_inter: false,
            ref_frame: [INTRA_FRAME, NONE_FRAME],
            interp_filter: 3, // SWITCHABLE sentinel (matches the decoder)
            ..Default::default()
        };
        // Interp-filter ceiling probe (env `VP9_IF_HARVEST`, observe-only): score the
        // winning inter MV's residual SSE under EIGHTTAP/SMOOTH/SHARP; accumulate the
        // per-block minimum vs EIGHTTAP. The reduction bounds the switchable-filter win.
        if best_inter.0.is_finite() && std::env::var("VP9_IF_HARVEST").is_ok() {
            use std::sync::atomic::Ordering::Relaxed;
            let (_j, slot, _rf, _mode, mv, _p) = best_inter;
            let save_ref = self.active_ref;
            let save_filt = self.active_filter;
            self.active_ref = slot;
            self.active_filter = 0;
            let s0 = self.pred_sse(mi_row, mi_col, mv, edges, bwl, bhl, f64::INFINITY);
            self.active_filter = 1;
            let s1 = self.pred_sse(mi_row, mi_col, mv, edges, bwl, bhl, f64::INFINITY);
            self.active_filter = 2;
            let s2 = self.pred_sse(mi_row, mi_col, mv, edges, bwl, bhl, f64::INFINITY);
            self.active_filter = save_filt;
            self.active_ref = save_ref;
            let smin = s0.min(s1).min(s2).max(0);
            IF_HARVEST[0].fetch_add(s0.max(0) as u64, Relaxed);
            IF_HARVEST[1].fetch_add(smin as u64, Relaxed);
            IF_HARVEST[2].fetch_add(1, Relaxed);
            if s1 < s0 || s2 < s0 {
                IF_HARVEST[3].fetch_add(1, Relaxed);
            }
        }
        let mut best_intra = (DC_PRED, f64::INFINITY);
        // Brick 2: only spend the 4 intra RD trials when inter isn't already good.
        let try_intra = !self.intra_gate || best_inter.0 >= self.lambda * self.intra_gate_t;
        if try_intra {
            for &m in &[DC_PRED, V_PRED, H_PRED, TM_PRED] {
                intra_mi.mode = m;
                let j = self.rd_cost_y(
                    &intra_mi, mi_row, mi_col, bsize, bwl, bhl, &snap, 8.0,
                    best_intra.1.min(best_inter.0),
                );
                if j < best_intra.1 {
                    best_intra = (m, j);
                }
            }
            // Chroma-RD: re-cost the winning intra mode on luma+chroma (with its chosen
            // UV mode) so the intra-vs-inter compare is on the SAME (yuv) basis — else
            // inter's added chroma cost would hand borderline blocks to intra unfairly.
            if self.chroma_rd {
                intra_mi.mode = best_intra.0;
                intra_mi.uv_mode = self.best_intra_mode(mi_row, mi_col, 1, bwl, bhl);
                // No abort here — this final re-cost needs the accurate full-YUV J for the
                // intra-vs-inter compare, and it runs once per block (negligible cost).
                best_intra.1 =
                    self.rd_cost_yuv(&intra_mi, mi_row, mi_col, bsize, bwl, bhl, 8.0, f64::INFINITY);
            }
        }

        let (best_j, best_slot, best_rf, best_mode, best_mv, mut predictor) = best_inter;
        // Three-way: single-inter (best_j) vs intra (best_intra.1) vs compound (compound_j).
        let compound_wins = compound_mi.is_some() && compound_j < best_j && compound_j < best_intra.1;
        let use_intra = !compound_wins && best_intra.1 < best_j;
        // Lock the chosen reference in for the trial reconstruct + the emit MC.
        let mut chosen = if compound_wins {
            let cmi = compound_mi.take().unwrap();
            let s0 = (cmi.ref_frame[0] as usize).wrapping_sub(1).min(2);
            self.active_ref = s0; // ref[0]'s slot (ref[1] handled per-ref in encode_plane)
            predictor = slot_pred[s0]; // for the returned modeinfo-cost estimate
            cmi
        } else if use_intra {
            self.active_ref = best_slot;
            let mut m = intra_mi;
            m.mode = best_intra.0;
            m
        } else {
            self.active_ref = best_slot;
            mk_inter(best_rf, best_mode, best_mv)
        };
        // Brick-1 plumbing probe: force every inter block to LAST+GOLDEN ZEROMV compound
        // (no MV bits, non-skip so encode_plane runs in both passes ⇒ no skip trap). Only
        // proves the compound header/emit/recon inverse; quality is irrelevant here.
        if self.compound_force && !use_intra {
            chosen.ref_frame = [LAST_FRAME, GOLDEN_FRAME];
            chosen.mode = ZEROMV;
            chosen.mv = [(0, 0), (0, 0)];
            self.active_ref = 0; // ref[0] = LAST (ref[1] handled per-ref in encode_plane)
        }
        // UV mode for the intra path (before the trial, so its chroma cost is right).
        if use_intra {
            chosen.uv_mode = self.best_intra_mode(mi_row, mi_col, 1, bwl, bhl);
        }
        // Switchable interp-filter RD (switchable frame, inter block only): pick the
        // filter minimising pred_sse + λ·filter_signal_bits for the winning MV. The
        // block's filter drives the trial reconstruct (active_filter) + the emit MC,
        // and is coded per block (ref→mode→FILTER→MV order) by encode_inter_block.
        if self.interp_filter == 4 && !use_intra {
            let above = self.above_mi(mi_row, mi_col);
            let left = self.left_mi(mi_row, mi_col);
            let ctx = switchable_interp_context(above.as_ref(), left.as_ref());
            let probs = self.fc.switchable_interp_prob[ctx];
            // Compound winners: score the actual averaged prediction at BOTH compound MVs,
            // not the single-ref best_mv (which is disconnected from the compound block).
            let comp = self.compound_filter && chosen.has_second_ref();
            let (cs0, cs1) = if comp {
                ((chosen.ref_frame[0] - 1) as usize, (chosen.ref_frame[1] - 1) as usize)
            } else {
                (0, 0)
            };
            let mut best = (0u8, f64::INFINITY);
            for f in 0u8..3 {
                self.active_filter = f;
                let sse = if comp {
                    self.pred_sse_compound(
                        mi_row, mi_col, chosen.mv[0], cs0, chosen.mv[1], cs1, edges, bwl, bhl,
                    )
                } else {
                    self.pred_sse(mi_row, mi_col, best_mv, edges, bwl, bhl, f64::INFINITY)
                };
                let bits = tree_bit_cost(&SWITCHABLE_INTERP_TREE, &probs, f as i32) as f64 / 256.0;
                let j = sse as f64 + self.lambda * bits;
                if j < best.1 {
                    best = (f, j);
                }
            }
            chosen.interp_filter = best.0;
            self.active_filter = best.0;
        }
        // Model-based early SKIP (CALIBRATED xsq gate): when the winner's residual is
        // small enough relative to the quantizer that the real rd_skip almost always
        // skips it (log2(xsq) ≥ model_skip_t, calibrated from a harvest), force skip
        // with an MC-only recon — avoiding this block's transform. A block below the
        // cutoff falls through to the normal transform (the real eob-based decision), so
        // mode_map's skip is emit-consistent. The mean-SSE / raw-model gates over-skipped
        // by skipping at LOW xsq (12–60% real-skip); the calibrated cutoff fires only
        // where real-skip is high (~91%), so false-skips are rare + small-residual.
        // The non-RD leaf fast path forces this gate on (with the same calibrated,
        // conservative cutoff) so variance-routed low-residual leaves skip the
        // transform — the second floor lever after LAST-ref-only.
        if (self.model_skip || fast) && !use_intra {
            // Compound winners: estimate the residual from the actual averaged prediction,
            // not the single-ref `pred_sse(best_mv)` (same fix as the interp-filter search —
            // a wrong residual estimate here forces wrong skip decisions on compound blocks).
            let sse = if self.compound_filter && chosen.has_second_ref() {
                self.pred_sse_compound(
                    mi_row, mi_col, chosen.mv[0], (chosen.ref_frame[0] - 1) as usize,
                    chosen.mv[1], (chosen.ref_frame[1] - 1) as usize, edges, bwl, bhl,
                )
            } else {
                self.pred_sse(mi_row, mi_col, best_mv, edges, bwl, bhl, f64::INFINITY)
            }
            .max(0) as u64;
            let n_log2 = (bwl + bhl + 4) as u32; // luma pixels = 2^(bwl+bhl+4)
            let xsq = varrd::model_xsq(sse, n_log2, self.dq_y.1 as i64);
            let log2xsq = 63 - xsq.max(1).leading_zeros();
            if log2xsq >= self.model_skip_t {
                chosen.tx_size = if self.use_tx_search {
                    MAX_TXSIZE[bsize] as u8
                } else {
                    self.base_tx(bsize)
                };
                self.last_trial_tx = chosen.tx_size;
                self.pending_eob = 0;
                self.force_skip = true;
                self.skip_trial = true;
                let mut sse_recon = 0u64;
                for plane in 0..3 {
                    let (_, s) =
                        self.encode_plane(None, &chosen, plane, mi_row, mi_col, bsize, bwl, bhl);
                    sse_recon += s;
                }
                self.skip_trial = false;
                self.force_skip = false;
                chosen.skip = true;
                return (chosen, predictor, 0, sse_recon);
            }
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
        self.pending_pred_sse = 0;
        self.skip_trial = true;
        for plane in 0..3 {
            let (b, s) = self.encode_plane(None, &chosen, plane, mi_row, mi_col, bsize, bwl, bhl);
            coef_bits += b;
            sse += s;
        }
        self.skip_trial = false;
        // Record the tx size the trial actually ran with — a skip block's cached
        // replay (mode_map) must re-run the trial at THIS size, not the max_tx the
        // syntax convention below substitutes.
        self.last_trial_tx = chosen.tx_size;
        let mut skip = !use_intra && self.pending_eob == 0;
        // RD skip decision (libvpx `x->skip`): when the residual is non-empty, drop it
        // whole if coding `skip` (recon == MC prediction, distortion = pred_sse) beats
        // coding the residual. Cascades into partitioning — a block that skips no longer
        // pulls SPLIT ahead of NONE — which is the static-content bit-rate win.
        if self.rd_skip && !use_intra && !skip {
            let above = self.above_mi(mi_row, mi_col);
            let left = self.left_mi(mi_row, mi_col);
            let sctx = skip_context(above.as_ref(), left.as_ref());
            let rate_skip = cost_bit(self.fc.skip_probs[sctx], 1) as f64 / 256.0;
            let rate_noskip =
                coef_bits as f64 / 256.0 + cost_bit(self.fc.skip_probs[sctx], 0) as f64 / 256.0;
            let j_skip = self.pending_pred_sse as f64 + self.lambda * rate_skip;
            let j_noskip = sse as f64 + self.lambda * rate_noskip;
            if j_skip <= j_noskip {
                skip = true;
                let pred_sse = self.pending_pred_sse;
                // Re-materialise the block as empty: MC prediction, no residual, zeroed
                // entropy contexts — bit-identical to a naturally-empty skip block.
                self.pending_eob = 0;
                self.force_skip = true;
                self.skip_trial = true;
                for plane in 0..3 {
                    self.encode_plane(None, &chosen, plane, mi_row, mi_col, bsize, bwl, bhl);
                }
                self.skip_trial = false;
                self.force_skip = false;
                sse = pred_sse;
            }
        }
        if skip {
            // A skip block codes no residual, but its tx_size still drives the
            // entropy-context width the decoder updates — so it must equal what the
            // decoder DERIVES (base_tx under ALLOW mode), not a hardcoded 4×4.
            chosen.tx_size = if self.use_tx_search {
                max_tx as u8
            } else {
                self.base_tx(bsize)
            };
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
    /// MV clamp bounds for a block — MUST match the decoder's `mb_to_edges` exactly.
    ///
    /// This used to hardcode `1` where the decoder subtracts the block's size in 8x8
    /// units (`bw8`/`bh8`), and ignored `bsize` entirely. The two agree only for 8x8
    /// blocks, where `bw8 == 1`. For a 64x64 block `bw8 == 8`, so the encoder let an MV
    /// point 7*64 = 448 eighth-pels further right than the decoder would allow: the
    /// encoder predicted from one position, the decoder clamped to another, and since
    /// the encoder feeds its own recon forward as the reference, the error compounded
    /// frame over frame.
    ///
    /// It only bites where the clamp actually binds — large blocks near the right or
    /// bottom edge with MVs pointing outward — which is why it showed up as right-edge
    /// damage on fast-panning content (park_joy_1080p50) and stayed invisible on quiet
    /// clips. Found with `VP9_RECON_CHECK`.
    fn block_edges(&self, mi_row: usize, mi_col: usize, bsize: usize) -> (i32, i32, i32, i32) {
        let bw8 = (1usize << B_WIDTH_LOG2[bsize] >> 1).max(1) as i32;
        let bh8 = (1usize << B_HEIGHT_LOG2[bsize] >> 1).max(1) as i32;
        let left = -((mi_col as i32 * 8) << 3);
        let right = (self.mi_cols as i32 - bw8 - mi_col as i32) * 8 * 8;
        let top = -((mi_row as i32 * 8) << 3);
        let bottom = (self.mi_rows as i32 - bh8 - mi_row as i32) * 8 * 8;
        (left, right, top, bottom)
    }

    /// Sum of absolute differences between the 8×8 source block at `(base_x,
    /// base_y)` and the reference shifted by integer pixels `(mv_r, mv_c)`,
    /// clamping reference reads to the plane border (as the MC convolver does).
    /// The currently-selected reference plane (LAST/GOLDEN/ALTREF per `active_ref`).
    #[inline]
    /// u8 mirror of the ACTIVE reference luma, built on first use per slot.
    /// Fetch ONCE per search/score call and pass the slice down — per-SAD Arc
    /// clones would cost millions of refcount atomics.
    fn ref8_active(&self) -> std::sync::Arc<[u8]> {
        let slot = self.active_ref;
        if let Some(r) = &self.refs8.borrow()[slot] {
            return r.clone();
        }
        let luma = &self.refs[slot].as_ref().unwrap()[0];
        let rc: std::sync::Arc<[u8]> = luma.buf.iter().map(|&v| v as u8).collect();
        self.refs8.borrow_mut()[slot] = Some(rc.clone());
        rc
    }

    /// The u8 search domain is usable when content is 8-bit and AVX2 is present.
    #[inline]
    fn u8_search(&self) -> bool {
        cfg!(target_arch = "x86_64") && self.has_avx2 && self.max_px == 255
    }

    fn aref(&self, plane: usize) -> &Plane {
        &self.refs[self.active_ref].as_ref().unwrap()[plane]
    }

    /// SAD for `mv` over the whole block (`bwl`/`bhl` in 4×4-log2 units), sampled
    /// as a grid of 8×8 tiles: every tile for ≤16×16, every other tile (stride 2)
    /// for 32×32/64×64 — bounding cost while still seeing the block's full-extent
    /// motion (scoring only the top-left 8×8 was a measured quality bug).
    /// Out-of-frame tiles (edge overhang) are skipped.
    /// Four candidates' block SADs in one tile pass (libvpx `sad_x4d` shape):
    /// each source tile is loaded once and scored against all four refs. Caller
    /// guarantees every candidate's full block extent is interior to the ref.
    /// Per-tile abort fires only when ALL four partials exceed `bound` — the
    /// sequential accept loop then sees exact-or-provably-losing values either
    /// way, so decisions are identical to four scalar calls.
    #[cfg(target_arch = "x86_64")]
    #[allow(clippy::too_many_arguments)]
    fn block_sad_sized_x4(
        &self,
        base_x: usize,
        base_y: usize,
        cands: [(i32, i32); 4],
        bwl: usize,
        bhl: usize,
        bound: i64,
        ref8: &[u8],
    ) -> [i64; 4] {
        let tiles_w = 1usize << bwl.saturating_sub(1);
        let tiles_h = 1usize << bhl.saturating_sub(1);
        let step = if tiles_w > 2 || tiles_h > 2 { 2 } else { 1 };
        let src = &self.src[0];
        let rp = self.aref(0);
        let mut sad = [0i64; 4];
        let mut ty = 0;
        while ty < tiles_h {
            let mut tx = 0;
            while tx < tiles_w {
                let (bx, by) = (base_x + tx * 8, base_y + ty * 8);
                if bx + 8 <= src.w && by + 8 <= src.h {
                    let refs: [*const u8; 4] = std::array::from_fn(|k| unsafe {
                        ref8.as_ptr().add(
                            (by as i32 + cands[k].0) as usize * rp.stride
                                + (bx as i32 + cands[k].1) as usize,
                        )
                    });
                    // SAFETY: caller checked full-block interior for every cand;
                    // AVX2 implied by the u8 search path.
                    let t = unsafe {
                        crate::inter::sad8x8_x4_u8(
                            self.src8.as_ptr().add(by * src.stride + bx),
                            src.stride,
                            refs,
                            rp.stride,
                        )
                    };
                    let mut min = i64::MAX;
                    for k in 0..4 {
                        sad[k] += t[k] as i64;
                        min = min.min(sad[k]);
                    }
                    if min > bound {
                        return [i64::MAX; 4];
                    }
                }
                tx += step;
            }
            ty += step;
        }
        sad
    }

    fn block_sad_sized(
        &self,
        base_x: usize,
        base_y: usize,
        mv_r: i32,
        mv_c: i32,
        bwl: usize,
        bhl: usize,
        bound: i64,
        ref8: Option<&[u8]>,
    ) -> i64 {
        let tiles_w = 1usize << bwl.saturating_sub(1); // 8-px tiles across
        let tiles_h = 1usize << bhl.saturating_sub(1);
        let step = if tiles_w > 2 || tiles_h > 2 { 2 } else { 1 };
        let src = &self.src[0];
        let mut sad = 0i64;
        let mut ty = 0;
        while ty < tiles_h {
            let mut tx = 0;
            while tx < tiles_w {
                let (bx, by) = (base_x + tx * 8, base_y + ty * 8);
                if bx + 8 <= src.w && by + 8 <= src.h {
                    sad += self.block_sad(bx, by, mv_r, mv_c, ref8);
                    // SAD accumulates non-negatively: once STRICTLY over the
                    // incumbent the candidate loses both the `<` and the `==`
                    // tie-break — identical decisions, remaining tiles skipped.
                    if sad > bound {
                        return i64::MAX;
                    }
                }
                tx += step;
            }
            ty += step;
        }
        sad
    }

    fn block_sad(
        &self,
        base_x: usize,
        base_y: usize,
        mv_r: i32,
        mv_c: i32,
        ref8: Option<&[u8]>,
    ) -> i64 {
        let src = &self.src[0];
        let rp = self.aref(0);
        let (rw, rh) = (rp.w as i32, rp.h as i32);
        let x0 = base_x as i32 + mv_c;
        let y0 = base_y as i32 + mv_r;
        // Fast interior path: the whole 8×8 window is in-bounds, so the per-pixel
        // clamp is a no-op — drop it. The branch-free inner loop auto-vectorizes,
        // and this covers the vast majority of searches (only frame-edge blocks with
        // a large MV fall to the clamped path). Byte-identical to the clamped loop.
        if x0 >= 0 && y0 >= 0 && x0 + 8 <= rw && y0 + 8 <= rh {
            let (x0, y0) = (x0 as usize, y0 as usize);
            #[cfg(target_arch = "x86_64")]
            if let Some(r8) = ref8 {
                // u8 search domain: bit-identical values, psadbw SAD.
                // SAFETY: interior window checked above; AVX2 implied by ref8.
                return unsafe {
                    crate::inter::sad8x8_u8(
                        self.src8.as_ptr().add(base_y * src.stride + base_x),
                        src.stride,
                        r8.as_ptr().add(y0 * rp.stride + x0),
                        rp.stride,
                    )
                } as i64;
            }
            let s = &src.buf[(base_y * src.stride + base_x)..];
            let r = &rp.buf[(y0 * rp.stride + x0)..];
            return sad8x8(s, src.stride, r, rp.stride, self.has_avx2) as i64;
        }
        // Edge path: clamp each sample to the coded region (unchanged semantics).
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
    /// `edges` is the block's UMV clamp window (loop-invariant across the subpel
    /// refinement — hoisted by the caller).
    fn predicted_sad(
        &self,
        mi_row: usize,
        mi_col: usize,
        mv: Mv,
        edges: (i32, i32, i32, i32),
    ) -> i64 {
        let base_x = mi_col * 8;
        let base_y = mi_row * 8;
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
        {
            let _s = prof::Scope::new(prof::S::Interp);
            predict_block(
                &refp,
                bx,
                by,
                (mv_q4.1 & 15) as usize,
                (mv_q4.0 & 15) as usize,
                self.active_filter as usize,
                &mut pred,
                8,
                8,
                8,
                false,
                self.max_px,
            );
        }
        let src = &self.src[0];
        // `pred` is the contiguous 8×8 MC prediction (stride 8); the source is strided.
        let s = &src.buf[(base_y * src.stride + base_x)..];
        sad8x8(s, src.stride, &pred, 8, self.has_avx2) as i64
    }

    /// Motion search on the luma block. First a full ±8-pixel integer window
    /// (around the zero MV and the predictor), then a 1/4-pel refinement around
    /// the integer best scored against the true 8-tap prediction. Ties break
    /// toward the shorter MV (fewer coded bits). Returns the MV in 1/8-pel.
    /// Both whole-pixel and 1/4-pel MVs keep the difference vs the (even)
    /// predictor even, so the `!allow_high_precision_mv` "hp = 1" invariant holds.
    fn search_mv(&self, mi_row: usize, mi_col: usize, predictor: Mv, bwl: usize, bhl: usize) -> Mv {
        // (A (row,col,ref,predictor,size)-keyed memo lived here; measured 0 hits
        // on every clip — mode_map replay upstream means no repeat asks — so the
        // per-search SipHash+insert was pure overhead and was removed.)
        // `VP9_CORNER_SAD` restores the old corner-only scoring (the A/B oracle).
        let (swl, shl) = if self.corner_sad { (1, 1) } else { (bwl, bhl) };
        // u8 search mirror, fetched ONCE per search (a per-eval fetch measurably
        // cost more than the psadbw kernel saved).
        let r8_arc = if self.u8_search() { Some(self.ref8_active()) } else { None };
        let ref8 = r8_arc.as_deref();
        let _s = prof::Scope::new(prof::S::MotionSearch);
        let base_x = mi_col * 8;
        let base_y = mi_row * 8;
        const RANGE: i32 = 8;
        let centers = [(0i32, 0i32), (predictor.0 / 8, predictor.1 / 8)];
        // NEARESTMV early-out: on the realtime leaf, if the predictor already fits well,
        // skip the whole diamond+subpel search and take it (NEWMV≈NEARESTMV ⇒ ~0 BD).
        if self.nonrd_me_skip > 0.0 && self.nonrd_leaf && self.variance_leaf {
            let (pr, pc) = (predictor.0 / 8, predictor.1 / 8);
            let psad = self.block_sad_sized(base_x, base_y, pr, pc, swl, shl, i64::MAX, ref8);
            let area = ((1i64 << bwl) * (1i64 << bhl) * 16) as f64; // luma pixels
            if (psad as f64) < self.nonrd_me_skip * area {
                return predictor;
            }
        }
        let mut best_px = (0i32, 0i32);
        let mut best_sad = i64::MAX;
        {
            let _s = prof::Scope::new(prof::S::IntSearch);
            // Full-block interior test (all scored tiles read the reference without
            // clamping) — the precondition for the x4 SIMD batch. Hoisted so BOTH the
            // exhaustive and the diamond integer search share it.
            let rp0 = self.aref(0);
            let (msw_px, msh_px) = ((1i32 << swl) * 4, (1i32 << shl) * 4);
            let interior = |r: i32, c: i32| -> bool {
                base_x as i32 + c >= 0
                    && base_y as i32 + r >= 0
                    && base_x as i32 + c + msw_px <= rp0.w as i32
                    && base_y as i32 + r + msh_px <= rp0.h as i32
            };
            if self.full_msearch {
                // Reference exhaustive search (±8 around zero + predictor) — the
                // oracle the diamond search is BD-rate-gated against (`VP9_FULL_MSEARCH`).
                // Interior runs of 4 go through the x4 SIMD batch (one source-tile load
                // scored against four ref positions) — byte-identical to four scalar
                // `block_sad_sized` calls but ~2-4× fewer tile loads; the exhaustive
                // path used to score one scalar position at a time. Edge tiles (where a
                // candidate overhangs the frame) fall back to the clamping scalar SAD.
                for &(cr, cc) in &centers {
                    for dr in -RANGE..=RANGE {
                        let r = cr + dr;
                        let mut dc = -RANGE;
                        while dc <= RANGE {
                            #[cfg(target_arch = "x86_64")]
                            if self.msearch_x4 && dc + 3 <= RANGE {
                                if let Some(r8) = ref8 {
                                    let cands: [(i32, i32); 4] =
                                        std::array::from_fn(|k| (r, cc + dc + k as i32));
                                    if cands.iter().all(|&(rr, cc2)| interior(rr, cc2)) {
                                        let sads = self.block_sad_sized_x4(
                                            base_x, base_y, cands, swl, shl, best_sad, r8,
                                        );
                                        for k in 0..4 {
                                            let (rr, cc2) = cands[k];
                                            let sad = sads[k];
                                            let shorter = rr.abs() + cc2.abs()
                                                < best_px.0.abs() + best_px.1.abs();
                                            if sad < best_sad || (sad == best_sad && shorter) {
                                                best_sad = sad;
                                                best_px = (rr, cc2);
                                            }
                                        }
                                        dc += 4;
                                        continue;
                                    }
                                }
                            }
                            let c = cc + dc;
                            let sad =
                                self.block_sad_sized(base_x, base_y, r, c, swl, shl, best_sad, ref8);
                            let shorter = r.abs() + c.abs() < best_px.0.abs() + best_px.1.abs();
                            if sad < best_sad || (sad == best_sad && shorter) {
                                best_sad = sad;
                                best_px = (r, c);
                            }
                            dc += 1;
                        }
                    }
                }
            } else {
                // Diamond (square-pattern step) search: evaluate the start candidates,
                // then refine with an 8-neighbour pattern at halving radii 8→4→2→1,
                // re-centering while it improves. ~30–50 SADs vs 578 exhaustive.
                // (A visited-set dedup was tried here and REVERTED: an integer
                // SAD is cheaper than the dedup scan — the check cost more than
                // the redundant work it saved. The subpel diamond keeps its dedup
                // because each of its scores is a full MC interpolation.)
                let mut consider = |best_sad: &mut i64, best_px: &mut (i32, i32), r: i32, c: i32| {
                    let sad =
                        self.block_sad_sized(base_x, base_y, r, c, swl, shl, *best_sad, ref8);
                    let shorter = r.abs() + c.abs() < best_px.0.abs() + best_px.1.abs();
                    if sad < *best_sad || (sad == *best_sad && shorter) {
                        *best_sad = sad;
                        *best_px = (r, c);
                        return true;
                    }
                    false
                };
                consider(&mut best_sad, &mut best_px, 0, 0);
                if centers[1] != (0, 0) {
                    consider(&mut best_sad, &mut best_px, centers[1].0, centers[1].1);
                }
                // (`interior` + `rp0` are hoisted above the full_msearch/diamond split.)
                let mut step = 8i32;
                while step >= 1 {
                    // Cap re-centerings per radius so the worst case stays bounded.
                    for _ in 0..8 {
                        let (cr, cc) = best_px;
                        let mut moved = false;
                        for quad in [
                            [(-step, 0), (step, 0), (0, -step), (0, step)],
                            [
                                (-step, -step),
                                (-step, step),
                                (step, -step),
                                (step, step),
                            ],
                        ] {
                            let all_interior =
                                quad.iter().all(|&(dr, dc)| interior(cr + dr, cc + dc));
                            #[cfg(target_arch = "x86_64")]
                            if all_interior {
                                if let Some(r8) = ref8 {
                                    let cands: [(i32, i32); 4] =
                                        std::array::from_fn(|k| (cr + quad[k].0, cc + quad[k].1));
                                    let sads = self.block_sad_sized_x4(
                                        base_x, base_y, cands, swl, shl, best_sad, r8,
                                    );
                                    for k in 0..4 {
                                        let (r, c) = cands[k];
                                        let sad = sads[k];
                                        let shorter = r.abs() + c.abs()
                                            < best_px.0.abs() + best_px.1.abs();
                                        if sad < best_sad || (sad == best_sad && shorter) {
                                            best_sad = sad;
                                            best_px = (r, c);
                                            moved = true;
                                        }
                                    }
                                    continue;
                                }
                            }
                            for (dr, dc) in quad {
                                moved |= consider(&mut best_sad, &mut best_px, cr + dr, cc + dc);
                            }
                        }
                        if !moved {
                            break;
                        }
                    }
                    step >>= 1;
                }
            }
        }
        // 1/4-pel refinement (even 1/8-pel offsets ⇒ delta stays even). The integer
        // best lies within ±1/2 pel of the true minimum, so ±4 covers it. The UMV
        // edges are loop-invariant — computed once for all 25 candidates. Scoring is
        // full-block (tiled, commit-identical clamp); `VP9_CORNER_SAD` restores the
        // corner-only scorer for the A/B oracle.
        let int = (best_px.0 * 8, best_px.1 * 8);
        let edges = self.block_edges(mi_row, mi_col, BLOCK_8X8);
        let mut best = int;
        let _s = prof::Scope::new(prof::S::SubpelSearch);
        let score = |mv: Mv, bound: i64| -> i64 {
            if self.corner_sad {
                self.predicted_sad(mi_row, mi_col, mv, edges)
            } else {
                self.predicted_sad_sized(mi_row, mi_col, mv, edges, bwl, bhl, bound, ref8)
            }
        };
        // The integer stage just scored `int` over the SAME tiles with the SAME
        // kernels; when the UMV clamp is an identity, the subpel-entry re-score
        // is bit-for-bit that value — reuse it (one full-block score saved per
        // search). `corner_sad` uses a different tiling, so it still re-scores.
        let (w_px, h_px) = ((1i32 << bwl) * 4, (1i32 << bhl) * 4);
        let clamped = clamp_mv_umv(int, w_px, h_px, 0, 0, edges);
        let mut best_sad = if !self.corner_sad && clamped == (int.0 * 2, int.1 * 2) {
            best_sad
        } else {
            score(int, i64::MAX)
        };
        let ip_sad = best_sad;
        // Provable skip: subpel SAD >= 0 and ties break toward the shorter MV
        // (the integer candidate), so nothing can beat a perfect integer match.
        if best_sad == 0 {
            return int;
        }
        if self.subpel_fast {
            // Preset lever: diamond at ½- then ¼-pel. Default = ONE round per
            // precision level (libvpx sub_pixel_tree shape, ≤8 scores); the
            // re-centering loop (`VP9_SUBPEL_ITER`) re-scores until no move
            // (~17 scores/search) for +BD oracle comparisons.
            let max_rounds = self.subpel_rounds;
            let mut visited = [(i32::MIN, i32::MIN); 32];
            visited[0] = int;
            let mut vlen = 1usize;
            // ⅛-pel refinement (hp-MV) ONLY when the predictor is in the high-precision
            // range — that is both the codability condition (else the ⅛-pel diff can't
            // be signalled) AND the content gate: high-motion blocks (large predictor)
            // stay at ¼-pel, so only low-motion blocks pay the extra subpel level.
            // NOTE: cutting subpel levels on the non-RD leaf was TRIED + REVERTED —
            // ½-pel-only cost +16% BD (mobile +32%) for only 1.05× (subpel MC is already
            // AVX2, so the wall barely moves while quarter-pel precision is load-bearing).
            let steps: &[i32] = if self.hp_mv && use_mv_hp(predictor) {
                &[4, 2, 1]
            } else {
                &[4, 2]
            };
            if self.subpel_diag {
                // Plus+diagonal tree (libvpx sub_pixel_tree geometry): one pass
                // per precision level — score the 4 plus points, then the corner
                // combining any improving horizontal+vertical directions. Reaches
                // diagonal optima in ONE level (the iterating plus-only diamond
                // needed 2+ re-center rounds, ~3 extra scores each).
                for &step in steps {
                    let (cr, cc) = best;
                    let mut mh = 0i32; // improving horizontal direction (dc)
                    let mut mv_ = 0i32; // improving vertical direction (dr)
                    for (dr, dc) in [(0, -step), (0, step), (-step, 0), (step, 0)] {
                        let cand = (cr + dr, cc + dc);
                        if visited[..vlen].contains(&cand) {
                            continue;
                        }
                        if vlen < visited.len() {
                            visited[vlen] = cand;
                            vlen += 1;
                        }
                        let sad = score(cand, best_sad);
                        let shorter = cand.0.abs() + cand.1.abs() < best.0.abs() + best.1.abs();
                        if sad < best_sad || (sad == best_sad && shorter) {
                            best_sad = sad;
                            best = cand;
                            if dr == 0 {
                                mh = dc;
                            } else {
                                mv_ = dr;
                            }
                        }
                    }
                    if mh != 0 && mv_ != 0 {
                        let cand = (cr + mv_, cc + mh);
                        if !visited[..vlen].contains(&cand) {
                            if vlen < visited.len() {
                                visited[vlen] = cand;
                                vlen += 1;
                            }
                            let sad = score(cand, best_sad);
                            let shorter =
                                cand.0.abs() + cand.1.abs() < best.0.abs() + best.1.abs();
                            if sad < best_sad || (sad == best_sad && shorter) {
                                best_sad = sad;
                                best = cand;
                            }
                        }
                    }
                }
                if self.g1_harvest {
                    eprintln!(
                        "G5 bw={} bh={} int_sad={} final_sad={} moved={}",
                        1 << bwl, 1 << bhl, ip_sad, best_sad, (best != int) as u8
                    );
                }
                return best;
            }
            let mut half_moved = false;
            for &step in steps {
                // Conditional tree-prune (`VP9_SUBPEL_TREE`): if the half-pel ring
                // never improved on the integer MV, the quarter ring almost never
                // does — skip it (libvpx-tree-like, but conditional not forced).
                if step == 2 && self.subpel_tree && !half_moved {
                    break;
                }
                let mut rounds = 0u32;
                loop {
                    let (cr, cc) = best;
                    let mut moved = false;
                    for (dr, dc) in [(-step, 0), (step, 0), (0, -step), (0, step)] {
                        let cand = (cr + dr, cc + dc);
                        if visited[..vlen].contains(&cand) {
                            continue; // already scored; cannot change the incumbent
                        }
                        if vlen < visited.len() {
                            visited[vlen] = cand;
                            vlen += 1;
                        }
                        let sad = score(cand, best_sad);
                        let shorter = cand.0.abs() + cand.1.abs() < best.0.abs() + best.1.abs();
                        if sad < best_sad || (sad == best_sad && shorter) {
                            best_sad = sad;
                            best = cand;
                            moved = true;
                            if step == 4 {
                                half_moved = true;
                            }
                        }
                    }
                    rounds += 1;
                    if !moved || (max_rounds > 0 && rounds >= max_rounds) {
                        break;
                    }
                }
            }
        } else {
            for dr in [-4i32, -2, 0, 2, 4] {
                for dc in [-4i32, -2, 0, 2, 4] {
                    if dr == 0 && dc == 0 {
                        continue;
                    }
                    let cand = (int.0 + dr, int.1 + dc);
                    let sad = score(cand, best_sad);
                    let shorter = cand.0.abs() + cand.1.abs() < best.0.abs() + best.1.abs();
                    if sad < best_sad || (sad == best_sad && shorter) {
                        best_sad = sad;
                        best = cand;
                    }
                }
            }
            // (⅛-pel refinement was tried here for the exhaustive-grid s0 path but
            // REVERTED: it introduced an hp×switchable conformance desync on some
            // content at s0 — the flat 8-neighbour ⅛-pel pass reached a MV the fast
            // diamond doesn't, and it desynced with the per-block filter. hp-MV keeps
            // its ⅛-pel refinement in the fast paths (s1+, incl. the s3 default),
            // which are conformant; s0 stays ¼-pel. See campaign memory.)
        }
        // G5 harvest: integer SAD vs what subpel refinement actually bought.
        if self.g1_harvest {
            eprintln!(
                "G5 bw={} bh={} int_sad={} final_sad={} moved={}",
                1 << bwl, 1 << bhl, ip_sad, best_sad, (best != int) as u8
            );
        }
        best
    }

    /// Full-block subpel SAD: MC + score `mv` over the whole block as sampled 8×8
    /// tiles (all tiles ≤16×16, stride 2 for 32/64), mirroring the commit MC
    /// exactly — real-dims `clamp_mv_umv` + per-tile `predict_block` at absolute
    /// coords (a slice of the whole-block prediction). Replaces the corner-only
    /// `predicted_sad`, which also clamped with (8,8) — a second mis-ranking.
    #[allow(clippy::too_many_arguments)]
    fn predicted_sad_sized(
        &self,
        mi_row: usize,
        mi_col: usize,
        mv: Mv,
        edges: (i32, i32, i32, i32),
        bwl: usize,
        bhl: usize,
        bound: i64,
        ref8: Option<&[u8]>,
    ) -> i64 {
        let base_x = mi_col * 8;
        let base_y = mi_row * 8;
        let (w, h) = ((1usize << bwl) * 4, (1usize << bhl) * 4);
        let mv_q4 = clamp_mv_umv(mv, w as i32, h as i32, 0, 0, edges);
        let bx0 = base_x as i32 + (mv_q4.1 >> 4);
        let by0 = base_y as i32 + (mv_q4.0 >> 4);
        let (fx, fy) = ((mv_q4.1 & 15) as usize, (mv_q4.0 & 15) as usize);
        let rp = self.aref(0);
        let refp = RefPlane {
            buf: &rp.buf,
            stride: rp.stride,
            w: rp.w as i32,
            h: rp.h as i32,
        };
        let src = &self.src[0];
        let (tiles_w, tiles_h) = (w / 8, h / 8);
        let step = if tiles_w > 2 || tiles_h > 2 { 2 } else { 1 };
        // u8 score domain (8-bit content): fused interpolate+psadbw per tile,
        // bit-identical to the u16 predict+SAD path (gated by
        // `u8_score_matches_u16_path`). Edge tiles fall back to the u16 path.
        // nr = 5 (not the u16 path's 4): the 16-wide u8 h-kernels use one
        // 16-byte window load reaching bx+12; edge tiles fall back to the
        // (value-identical) u16 path.
        let (nl, nr) = if fx != 0 { (3i32, 5i32) } else { (0, 0) };
        let (nt, nb) = if fy != 0 { (3i32, 4i32) } else { (0, 0) };
        // Hoisted out of the tile loop: the u8 plane view and the profiler scope
        // are per-CALL invariants (building them per tile was measurable glue).
        #[cfg(target_arch = "x86_64")]
        let refp8_c = ref8.map(|r8| crate::inter::RefPlane8 {
            buf: r8,
            stride: rp.stride,
            w: refp.w,
            h: refp.h,
        });
        let _s_call = prof::Scope::new(prof::S::Interp);
        let mut pred = [0u16; 64];
        let mut sad = 0i64;
        let mut ty = 0;
        while ty < tiles_h {
            let mut tx = 0;
            while tx < tiles_w {
                let (px, py) = (base_x + tx * 8, base_y + ty * 8);
                if px + 8 <= src.w && py + 8 <= src.h {
                    let (bx, by) = (bx0 + (tx * 8) as i32, by0 + (ty * 8) as i32);
                    #[cfg(target_arch = "x86_64")]
                    if let Some(refp8) = &refp8_c {
                        // Bilinear scorer window is smaller ((bx,by)..(bx+9,by+9))
                        // so MORE tiles qualify than the 8-tap's filtered window.
                        if self.subpel_bilinear
                            && bx >= 0
                            && bx + 9 <= refp.w
                            && by >= 0
                            && by + 9 <= refp.h
                        {
                            // SAFETY: window checked; AVX2 implied by u8_search.
                            sad += unsafe {
                                crate::inter::subpel_bilinear_score8x8_u8(
                                    refp8,
                                    bx,
                                    by,
                                    fx,
                                    fy,
                                    self.src8.as_ptr().add(py * src.stride + px),
                                    src.stride,
                                )
                            } as i64;
                            if sad > bound {
                                return i64::MAX;
                            }
                            tx += step;
                            continue;
                        }
                        if bx - nl >= 0
                            && bx + 8 + nr <= refp.w
                            && by - nt >= 0
                            && by + 8 + nb <= refp.h
                        {
                            // SAFETY: window checked in-bounds; AVX2 implied by
                            // `u8_search`; src8 covers the same plane as src.
                            sad += unsafe {
                                crate::inter::subpel_score8x8_u8(
                                    refp8,
                                    bx,
                                    by,
                                    fx,
                                    fy,
                                    self.active_filter as usize,
                                    self.src8.as_ptr().add(py * src.stride + px),
                                    src.stride,
                                )
                            } as i64;
                            if sad > bound {
                                return i64::MAX;
                            }
                            tx += step;
                            continue;
                        }
                    }
                    predict_block(
                        &refp,
                        bx,
                        by,
                        fx,
                        fy,
                        self.active_filter as usize,
                        &mut pred,
                        8,
                        8,
                        8,
                        false,
                        self.max_px,
                    );
                    let s = &src.buf[(py * src.stride + px)..];
                    sad += sad8x8(s, src.stride, &pred, 8, self.has_avx2) as i64;
                    // Same provable abort as block_sad_sized: strictly over the
                    // incumbent means this MV cannot win, so its remaining tiles
                    // (and their interpolation, the dominant cost) are skipped.
                    if sad > bound {
                        return i64::MAX;
                    }
                }
                tx += step;
            }
            ty += step;
        }
        sad
    }

    /// Full-block prediction SSE for an inter `mv` — the distortion term of the
    /// SKIP RD cost `J_skip = pred_SSE + λ·bits` used to shortlist mode candidates
    /// (same units as `rd_cost_y`'s `J`, so it ranks bit-cheap predicted-MV modes
    /// against NEWMV correctly — a pure SAD would not). Cheap: MC + Σ(src−pred)²,
    /// NO forward transform / quantize / trellis. Full block (no tile sampling) so
    /// the SSE scale matches `λ·bits`.
    fn pred_sse(
        &self,
        mi_row: usize,
        mi_col: usize,
        mv: Mv,
        edges: (i32, i32, i32, i32),
        bwl: usize,
        bhl: usize,
        bound: f64,
    ) -> i64 {
        let _s = prof::Scope::new(prof::S::PredSse);
        let base_x = mi_col * 8;
        let base_y = mi_row * 8;
        let (w, h) = ((1usize << bwl) * 4, (1usize << bhl) * 4);
        let mv_q4 = clamp_mv_umv(mv, w as i32, h as i32, 0, 0, edges);
        let bx0 = base_x as i32 + (mv_q4.1 >> 4);
        let by0 = base_y as i32 + (mv_q4.0 >> 4);
        let (fx, fy) = ((mv_q4.1 & 15) as usize, (mv_q4.0 & 15) as usize);
        let rp = self.aref(0);
        let refp = RefPlane {
            buf: &rp.buf,
            stride: rp.stride,
            w: rp.w as i32,
            h: rp.h as i32,
        };
        let src = &self.src[0];
        let (tiles_w, tiles_h) = (w / 8, h / 8);
        // Shortlist scoring only ranks candidates, so a 2× tile stride (with the SSE
        // scaled back to full-block magnitude for the λ·bits comparison) trades a
        // little ranking precision for ~4× fewer interps on 16×16+ blocks.
        let step = if self.motion_fast && (tiles_w > 1 || tiles_h > 1) { 2 } else { 1 };
        // u8 SSE domain (8-bit content): fused interpolate + squared error per
        // tile, bit-identical (gated by `u8_sse_matches_u16_path`) — this also
        // replaces the SCALAR per-pixel d² loop below. Edge tiles fall back.
        let r8_arc = if self.u8_search() { Some(self.ref8_active()) } else { None };
        let (nl, nr) = if fx != 0 { (3i32, 5i32) } else { (0, 0) };
        let (nt, nb) = if fy != 0 { (3i32, 4i32) } else { (0, 0) };
        let mut pred = [0u16; 64];
        let mut sse = 0i64;
        let mut sampled = 0i64;
        let mut ty = 0;
        while ty < tiles_h {
            let mut tx = 0;
            while tx < tiles_w {
                let (px, py) = (base_x + tx * 8, base_y + ty * 8);
                if px + 8 <= src.w && py + 8 <= src.h {
                    sampled += 1;
                    let _s = prof::Scope::new(prof::S::Interp);
                    let (bx, by) = (bx0 + (tx * 8) as i32, by0 + (ty * 8) as i32);
                    #[cfg(target_arch = "x86_64")]
                    if let Some(r8) = &r8_arc {
                        if bx - nl >= 0
                            && bx + 8 + nr <= refp.w
                            && by - nt >= 0
                            && by + 8 + nb <= refp.h
                        {
                            let refp8 = crate::inter::RefPlane8 {
                                buf: r8,
                                stride: rp.stride,
                                w: refp.w,
                                h: refp.h,
                            };
                            // SAFETY: window checked; AVX2 implied by u8_search;
                            // src8 mirrors src exactly for 8-bit content.
                            sse += unsafe {
                                crate::inter::subpel_sse8x8_u8(
                                    &refp8,
                                    bx,
                                    by,
                                    fx,
                                    fy,
                                    self.active_filter as usize,
                                    self.src8.as_ptr().add(py * src.stride + px),
                                    src.stride,
                                )
                            } as i64;
                            if (sse as f64) > bound {
                                return i64::MAX;
                            }
                            tx += step;
                            continue;
                        }
                    }
                    predict_block(
                        &refp,
                        bx,
                        by,
                        fx,
                        fy,
                        self.active_filter as usize,
                        &mut pred,
                        8,
                        8,
                        8,
                        false,
                        self.max_px,
                    );
                    let s = &src.buf[(py * src.stride + px)..];
                    for r in 0..8 {
                        for c in 0..8 {
                            let d = s[r * src.stride + c] as i64 - pred[r * 8 + c] as i64;
                            sse += d * d;
                        }
                    }
                    // Shortlist abort: SSE accumulates non-negatively, so once
                    // STRICTLY over the bound this candidate provably cannot
                    // enter the top-K (the bound only shrinks as more collect) —
                    // identical top-K set, remaining tiles (and their MC) skipped.
                    if (sse as f64) > bound {
                        return i64::MAX;
                    }
                }
                tx += step;
            }
            ty += step;
        }
        // Scale the sampled SSE back to full-block magnitude so J_skip stays on the
        // same scale as `rd_cost_y`'s full-block SSE (+ λ·bits).
        let total: i64 = {
            let mut n = 0i64;
            let mut yy = 0;
            while yy < tiles_h {
                let mut xx = 0;
                while xx < tiles_w {
                    let (px, py) = (base_x + xx * 8, base_y + yy * 8);
                    if px + 8 <= src.w && py + 8 <= src.h {
                        n += 1;
                    }
                    xx += 1;
                }
                yy += 1;
            }
            n
        };
        if sampled > 0 && total > sampled {
            sse = sse * total / sampled;
        }
        sse
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
        avg: bool,
    ) {
        let _s = prof::Scope::new(prof::S::Mc);
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
            self.active_filter as usize,
            &mut self.rec[plane].buf[dst_off..],
            stride,
            w,
            h,
            avg,
            self.max_px,
        );
    }

    /// Sub-8×8 motion compensation for one plane — the exact inverse of the decoder's
    /// `inter_predict_plane` sub-8×8 branch (non-scaled `mc_one`): MC each 4×4 sub-block
    /// in raster order using `average_split_mvs` (luma = `bmi_mv[i]`; chroma = the 2/4-way
    /// average), clamping each MV with the FULL block `bw/bh` (not the 4×4), exactly as the
    /// decoder does. Single reference (`active_ref`), no compound — matches our P frames.
    fn inter_predict_sub8x8(
        &mut self,
        plane: usize,
        mi: &ModeInfo,
        mi_row: usize,
        mi_col: usize,
        bsize: usize,
        bwl: usize,
        bhl: usize,
    ) {
        let (ss_x, ss_y) = (self.rec[plane].ss_x, self.rec[plane].ss_y);
        let base_x = (mi_col * MI_SIZE) >> ss_x;
        let base_y = (mi_row * MI_SIZE) >> ss_y;
        let n4_w = (1usize << bwl) >> ss_x;
        let n4_h = (1usize << bhl) >> ss_y;
        let (bw, bh) = ((n4_w * 4) as i32, (n4_h * 4) as i32);
        let edges = self.block_edges(mi_row, mi_col, bsize);
        let stride = self.rec[plane].stride;
        let rp = &self.refs[self.active_ref].as_ref().unwrap()[plane];
        let refp = RefPlane {
            buf: &rp.buf,
            stride: rp.stride,
            w: rp.w as i32,
            h: rp.h as i32,
        };
        let mut i = 0;
        for y in 0..n4_h {
            for x in 0..n4_w {
                let mv = average_split_mvs(mi, 0, i, ss_x, ss_y);
                let mv_q4 = clamp_mv_umv(mv, bw, bh, ss_x, ss_y, edges);
                let (dst_x, dst_y) = (base_x + x * 4, base_y + y * 4);
                let bx = dst_x as i32 + (mv_q4.1 >> 4);
                let by = dst_y as i32 + (mv_q4.0 >> 4);
                let subpel_x = (mv_q4.1 & 15) as usize;
                let subpel_y = (mv_q4.0 & 15) as usize;
                let dst_off = dst_y * stride + dst_x;
                predict_block(
                    &refp,
                    bx,
                    by,
                    subpel_x,
                    subpel_y,
                    self.active_filter as usize,
                    &mut self.rec[plane].buf[dst_off..],
                    stride,
                    4,
                    4,
                    false,
                    self.max_px,
                );
                i += 1;
            }
        }
    }

    /// SAD of one 4×4 luma sub-block (at 4-pel offset `sub_x,sub_y` within the block)
    /// against its motion-compensated prediction for `mv`, clamping with the full
    /// block `bw/bh` exactly as `mc_one` does. Drives the sub-8×8 per-sub-block search.
    #[allow(clippy::too_many_arguments, dead_code)]
    fn sub4x4_sad(
        &self,
        mi_row: usize,
        mi_col: usize,
        sub_x: usize,
        sub_y: usize,
        mv: Mv,
        bw: i32,
        bh: i32,
        edges: (i32, i32, i32, i32),
    ) -> i64 {
        let base_x = mi_col * 8 + sub_x * 4;
        let base_y = mi_row * 8 + sub_y * 4;
        let mv_q4 = clamp_mv_umv(mv, bw, bh, 0, 0, edges);
        let bx = base_x as i32 + (mv_q4.1 >> 4);
        let by = base_y as i32 + (mv_q4.0 >> 4);
        let rp = self.aref(0);
        // Fast path: integer MV (no subpel fraction) + in-bounds 4×4 window → direct
        // SAD, skipping the 8-tap `predict_block` (which for frac=0 is a plain copy).
        // Byte-identical; covers the bulk of the integer search grid.
        if (mv_q4.0 & 15) == 0
            && (mv_q4.1 & 15) == 0
            && bx >= 0
            && by >= 0
            && bx + 4 <= rp.w as i32
            && by + 4 <= rp.h as i32
        {
            let (bx, by) = (bx as usize, by as usize);
            let sp = &self.src[0];
            let s = &sp.buf[(base_y * sp.stride + base_x)..];
            let rr = &rp.buf[(by * rp.stride + bx)..];
            // 4×4 is too small for AVX2 to pay (the horizontal reduction dominates 16
            // elements) — the scalar branchless kernel is faster. Measured 2026-07-09.
            return sad4x4_scalar(s, sp.stride, rr, rp.stride) as i64;
        }
        let refp = RefPlane {
            buf: &rp.buf,
            stride: rp.stride,
            w: rp.w as i32,
            h: rp.h as i32,
        };
        let mut pred = [0u16; 16];
        predict_block(
            &refp,
            bx,
            by,
            (mv_q4.1 & 15) as usize,
            (mv_q4.0 & 15) as usize,
            self.active_filter as usize,
            &mut pred,
            4,
            4,
            4,
            false,
            self.max_px,
        );
        let sp = &self.src[0];
        let mut sad = 0i64;
        for r in 0..4 {
            for c in 0..4 {
                let s = sp.buf[(base_y + r) * sp.stride + base_x + c] as i64;
                sad += (s - pred[r * 4 + c] as i64).abs();
            }
        }
        sad
    }

    /// SAD of one 4×4 luma sub-block vs the AVERAGED compound prediction
    /// `(slot0@mv0 + slot1@mv1 + 1)>>1` — the sub-8×8 compound scorer.
    #[allow(clippy::too_many_arguments)]
    fn sub4x4_sad_compound(
        &self,
        mi_row: usize,
        mi_col: usize,
        sub_x: usize,
        sub_y: usize,
        mv0: Mv,
        slot0: usize,
        mv1: Mv,
        slot1: usize,
        bw: i32,
        bh: i32,
        edges: (i32, i32, i32, i32),
    ) -> i64 {
        let base_x = mi_col * 8 + sub_x * 4;
        let base_y = mi_row * 8 + sub_y * 4;
        let filt = self.active_filter as usize;
        let mut buf = [0u16; 16];
        for (i, (mv, slot)) in [(mv0, slot0), (mv1, slot1)].iter().enumerate() {
            let q = clamp_mv_umv(*mv, bw, bh, 0, 0, edges);
            let rp = &self.refs[*slot].as_ref().unwrap()[0];
            let refp = RefPlane { buf: &rp.buf, stride: rp.stride, w: rp.w as i32, h: rp.h as i32 };
            predict_block(
                &refp, base_x as i32 + (q.1 >> 4), base_y as i32 + (q.0 >> 4),
                (q.1 & 15) as usize, (q.0 & 15) as usize, filt, &mut buf, 4, 4, 4, i == 1, self.max_px,
            );
        }
        let sp = &self.src[0];
        let mut sad = 0i64;
        for r in 0..4 {
            let sr = (base_y + r) * sp.stride + base_x;
            for c in 0..4 {
                sad += (buf[r * 4 + c] as i64 - sp.buf[sr + c] as i64).abs();
            }
        }
        sad
    }

    /// Accumulate the sub-8×8 compound ceiling probe for one finalized sub-block: single-ref
    /// best SAD vs the best of three cheap compound partners on the fixed ref (ZEROMV, the
    /// same MV, the NEGATED MV = bi-directional guess). Observe-only.
    #[allow(clippy::too_many_arguments)]
    fn sub8_probe_acc(
        &self,
        mi_row: usize,
        mi_col: usize,
        idx: usize,
        idy: usize,
        best_mv: Mv,
        fixed_slot: usize,
        bw: i32,
        bh: i32,
        edges: (i32, i32, i32, i32),
    ) {
        use std::sync::atomic::Ordering::Relaxed;
        let single = self.sub4x4_sad(mi_row, mi_col, idx, idy, best_mv, bw, bh, edges);
        let neg = (-best_mv.0, -best_mv.1);
        let c0 = self.sub4x4_sad_compound(mi_row, mi_col, idx, idy, best_mv, 0, (0, 0), fixed_slot, bw, bh, edges);
        let c1 = self.sub4x4_sad_compound(mi_row, mi_col, idx, idy, best_mv, 0, best_mv, fixed_slot, bw, bh, edges);
        let c2 = self.sub4x4_sad_compound(mi_row, mi_col, idx, idy, best_mv, 0, neg, fixed_slot, bw, bh, edges);
        let comp = c0.min(c1).min(c2);
        SUB8_PROBE[0].fetch_add(single as u64, Relaxed);
        SUB8_PROBE[1].fetch_add(comp.min(single) as u64, Relaxed);
        SUB8_PROBE[2].fetch_add(1, Relaxed);
        if comp < single {
            SUB8_PROBE[3].fetch_add(1, Relaxed);
        }
    }

    /// Integer ±4 (around zero + `predictor`) then ¼-pel motion search for one 4×4
    /// luma sub-block. Returns the best 1/8-pel MV.
    #[allow(clippy::too_many_arguments, dead_code)]
    fn search_mv_sub(
        &self,
        mi_row: usize,
        mi_col: usize,
        sub_x: usize,
        sub_y: usize,
        predictor: Mv,
        bw: i32,
        bh: i32,
        edges: (i32, i32, i32, i32),
    ) -> Mv {
        const R: i32 = 4;
        let mut best = (0i32, 0i32);
        let mut best_sad = i64::MAX;
        if self.full_msearch {
            // Reference exhaustive ±4 (oracle for the diamond, `VP9_FULL_MSEARCH`).
            for &(cr, cc) in &[(0i32, 0i32), (predictor.0 / 8, predictor.1 / 8)] {
                for dr in -R..=R {
                    for dc in -R..=R {
                        let mv = ((cr + dr) * 8, (cc + dc) * 8);
                        let sad = self.sub4x4_sad(mi_row, mi_col, sub_x, sub_y, mv, bw, bh, edges);
                        if sad < best_sad {
                            best_sad = sad;
                            best = mv;
                        }
                    }
                }
            }
        } else {
            // Diamond: start candidates, then the 8-neighbour pattern at radii 4→2→1
            // (~15–25 SADs vs 162 exhaustive). Whole-pel candidates, 1/8-pel units.
            let mut consider = |best_sad: &mut i64, best: &mut Mv, r: i32, c: i32| {
                let mv = (r * 8, c * 8);
                let sad = self.sub4x4_sad(mi_row, mi_col, sub_x, sub_y, mv, bw, bh, edges);
                if sad < *best_sad {
                    *best_sad = sad;
                    *best = mv;
                    return true;
                }
                false
            };
            consider(&mut best_sad, &mut best, 0, 0);
            let p = (predictor.0 / 8, predictor.1 / 8);
            if p != (0, 0) {
                consider(&mut best_sad, &mut best, p.0, p.1);
            }
            let mut step = 4i32;
            while step >= 1 {
                for _ in 0..8 {
                    let (cr, cc) = (best.0 / 8, best.1 / 8);
                    let mut moved = false;
                    for (dr, dc) in [
                        (-step, 0),
                        (step, 0),
                        (0, -step),
                        (0, step),
                        (-step, -step),
                        (-step, step),
                        (step, -step),
                        (step, step),
                    ] {
                        moved |= consider(&mut best_sad, &mut best, cr + dr, cc + dc);
                    }
                    if !moved {
                        break;
                    }
                }
                step >>= 1;
            }
        }
        let int = best;
        for dr in [-4i32, -2, 0, 2, 4] {
            for dc in [-4i32, -2, 0, 2, 4] {
                let mv = (int.0 + dr, int.1 + dc);
                let sad = self.sub4x4_sad(mi_row, mi_col, sub_x, sub_y, mv, bw, bh, edges);
                if sad < best_sad {
                    best_sad = sad;
                    best = mv;
                }
            }
        }
        best
    }

    /// Sub-8×8 per-sub-block decision (Part B). For `subsize` (BLOCK_4X4/8X4/4X8) it
    /// decides each 4×4 sub-block's inter mode + MV in raster order — mirroring the
    /// decoder's `bmi`/`bmi_mv` fill so the reconstruction matches bit-exactly — using
    /// `enc_sub8x8_mv` for the NEAREST/NEAR predictors (which read earlier sub-blocks)
    /// and a per-sub-block search for NEWMV. Mode chosen by SAD + a NEWMV MV-bit penalty
    /// (so free predicted MVs win on ties). Then reconstructs + costs the residual (4×4
    /// tx) via `encode_plane`, which dispatches to `inter_predict_sub8x8`. Single ref
    /// (LAST). Returns `(mode_info, coef_bits, sse)`.
    /// Run the per-4×4 sub-8×8 mode/MV search for ONE reference `rf` (slot `slot`), filling
    /// `bmi`/`bmi_mv`. Returns the decided ModeInfo + the summed best-mode cost (SAD, plus the
    /// NEWMV bit-penalty) — the proxy the multi-ref pick uses to compare references.
    #[allow(clippy::too_many_arguments)]
    fn sub8x8_search_ref(
        &mut self,
        mi_row: usize,
        mi_col: usize,
        bsize: usize,
        num_4x4_w: usize,
        num_4x4_h: usize,
        bw: i32,
        bh: i32,
        edges: (i32, i32, i32, i32),
        rf: i8,
        slot: usize,
        do_probe: bool,
        fixed_slot: usize,
    ) -> (ModeInfo, i64) {
        // Pure SAD search — opens no other scope, so this is a disjoint partition
        // member and can be summed alongside the transform kernels. Its parent
        // `decide_sub8x8` is inclusive of a whole residual trial and is therefore
        // an `[i]` diagnostic only.
        let _s = prof::Scope::new(prof::S::Sub8x8Search);
        // A NEWMV must beat a predicted mode by ~this SAD to justify its MV bits.
        const NEWMV_SAD_PENALTY: i64 = 48;
        self.active_ref = slot;
        let mut mi = ModeInfo {
            sb_type: bsize as u8,
            skip: false,
            tx_size: 0,
            is_inter: true,
            ref_frame: [rf, NONE_FRAME],
            interp_filter: self.active_filter as u8,
            ..Default::default()
        };
        let mut last_mode = ZEROMV;
        let mut total = 0i64;
        let mut idy = 0;
        while idy < 2 {
            let mut idx = 0;
            while idx < 2 {
                let j = idy * 2 + idx;
                // ZEROMV baseline.
                let mut best_cost = self.sub4x4_sad(mi_row, mi_col, idx, idy, (0, 0), bw, bh, edges);
                let mut best_mode = ZEROMV;
                let mut best_mv = (0i32, 0i32);
                // NEAREST / NEAR — free predicted MVs (no MV bits).
                for m in [NEARESTMV, NEARMV] {
                    let mv = lower_mv_precision(
                        self.enc_sub8x8_mv(&mi, m, j, 0, mi_row, mi_col, bsize, edges),
                        self.hp_mv,
                    );
                    let c = self.sub4x4_sad(mi_row, mi_col, idx, idy, mv, bw, bh, edges);
                    if c < best_cost {
                        best_cost = c;
                        best_mode = m;
                        best_mv = mv;
                    }
                }
                // NEWMV — searched, charged the MV-bit penalty. Pre-screen: when a free
                // predicted mode already fits this 4×4 well, skip the search.
                if self.sub8x8_prescreen > 0 && best_cost <= self.sub8x8_prescreen {
                    if do_probe {
                        self.sub8_probe_acc(mi_row, mi_col, idx, idy, best_mv, fixed_slot, bw, bh, edges);
                    }
                    mi.bmi[j] = best_mode;
                    mi.bmi_mv[j] = [best_mv, (0, 0)];
                    if num_4x4_h == 2 {
                        mi.bmi[j + 2] = best_mode;
                        mi.bmi_mv[j + 2] = [best_mv, (0, 0)];
                    }
                    if num_4x4_w == 2 {
                        mi.bmi[j + 1] = best_mode;
                        mi.bmi_mv[j + 1] = [best_mv, (0, 0)];
                    }
                    total += best_cost;
                    last_mode = best_mode;
                    idx += num_4x4_w;
                    continue;
                }
                let (cand, _) = find_mv_refs(
                    &self.mi, self.mi_cols, self.mi_rows, self.tile_start, self.tile_end,
                    mi_row, mi_col, bsize, rf, &self.sign_bias, NEWMV, -1, edges,
                    self.prev_mv(mi_row, mi_col),
                );
                let pred = lower_mv_precision(cand[0], self.hp_mv);
                let mvw = self.search_mv_sub(mi_row, mi_col, idx, idy, pred, bw, bh, edges);
                let cw = self.sub4x4_sad(mi_row, mi_col, idx, idy, mvw, bw, bh, edges)
                    + NEWMV_SAD_PENALTY;
                // G4 harvest (observe-only): predicted-mode SAD vs the searched NEWMV.
                if self.g1_harvest {
                    eprintln!("G4 pred_sad={} newmv_cost={} won={}", best_cost, cw, (cw < best_cost) as u8);
                }
                if cw < best_cost {
                    best_mode = NEWMV;
                    best_mv = mvw;
                    best_cost = cw;
                }
                if do_probe {
                    self.sub8_probe_acc(mi_row, mi_col, idx, idy, best_mv, fixed_slot, bw, bh, edges);
                }
                mi.bmi[j] = best_mode;
                mi.bmi_mv[j] = [best_mv, (0, 0)];
                if num_4x4_h == 2 {
                    mi.bmi[j + 2] = best_mode;
                    mi.bmi_mv[j + 2] = [best_mv, (0, 0)];
                }
                if num_4x4_w == 2 {
                    mi.bmi[j + 1] = best_mode;
                    mi.bmi_mv[j + 1] = [best_mv, (0, 0)];
                }
                total += best_cost;
                last_mode = best_mode;
                idx += num_4x4_w;
            }
            idy += num_4x4_h;
        }
        mi.mode = last_mode;
        mi.mv = mi.bmi_mv[3];
        (mi, total)
    }

    fn decide_sub8x8(
        &mut self,
        mi_row: usize,
        mi_col: usize,
        subsize: usize,
        bwl: usize,
        bhl: usize,
        keep_recon: bool,
    ) -> (ModeInfo, u64, u64) {
        let _s = prof::Scope::new(prof::S::Sub8x8);
        let bsize = subsize;
        let num_4x4_w = 1usize << B_WIDTH_LOG2[bsize];
        let num_4x4_h = 1usize << B_HEIGHT_LOG2[bsize];
        let edges = self.block_edges(mi_row, mi_col, bsize);
        let (bw, bh) = (((1i32 << bwl) * 4), ((1i32 << bhl) * 4));
        // Sub-8×8 compound ceiling probe: the fixed compound ref's slot (comp_fixed_ref-1),
        // and whether to accumulate the SAD-reduction stats (observe-only).
        let fixed_slot = (self.fc.comp_fixed_ref as usize).wrapping_sub(1);
        let probe = self.sub8_probe
            && self.compound
            && fixed_slot < 3
            && self.refs[fixed_slot].is_some();
        // LAST first (always). Its summed sub-block SAD is the cheap content signal: only
        // pay the GOLDEN/ALTREF search when LAST fits POORLY (gate) — static/well-predicted
        // leaves stay LAST-only (byte-identical, no extra cost), so the ~3× search lands only
        // on hard leaves. A better single ref adds NO MV bits, just less residual; the SAD
        // proxy under-prices non-LAST, so a margin penalty guards against equal-residual flips.
        let (last_mi, last_total) = self.sub8x8_search_ref(
            mi_row, mi_col, bsize, num_4x4_w, num_4x4_h, bw, bh, edges,
            LAST_FRAME, 0, probe, fixed_slot,
        );
        let mut mi = last_mi;
        if self.sub8x8_multiref && (last_total as f64) > self.sub8x8_multiref_gate {
            let mut best_cost = last_total as f64 + self.lambda; // LAST: one ref bool
            for &(cand_rf, cand_slot) in &[(GOLDEN_FRAME, 1usize), (ALTREF_FRAME, 2usize)] {
                if self.refs[cand_slot].is_none() {
                    continue;
                }
                let (m, total) = self.sub8x8_search_ref(
                    mi_row, mi_col, bsize, num_4x4_w, num_4x4_h, bw, bh, edges,
                    cand_rf, cand_slot, false, fixed_slot,
                );
                let cost = total as f64 + self.lambda * 2.0 + self.sub8x8_ref_penalty;
                if cost < best_cost {
                    best_cost = cost;
                    mi = m;
                }
            }
        }
        // Lock active_ref to the chosen reference for the residual trial + emit recon.
        self.active_ref = (mi.ref_frame[0] as usize) - 1;
        // Residual trial (4×4 tx); encode_plane MCs via inter_predict_sub8x8. Same
        // snap/keep_recon contract as decide_inter: skip keeps the MC-only recon; a
        // non-skip block rolls back (emit re-reconstructs) unless the caller keeps it.
        let start = self.snap_block(mi_row, mi_col, bwl, bhl);
        self.pending_eob = 0;
        self.skip_trial = true;
        let (mut coef_bits, mut sse) = (0u64, 0u64);
        for plane in 0..3 {
            let (b, s) = self.encode_plane(None, &mi, plane, mi_row, mi_col, bsize, bwl, bhl);
            coef_bits += b;
            sse += s;
        }
        self.skip_trial = false;
        self.last_trial_tx = mi.tx_size; // sub-8×8 tx is 4×4; unchanged by skip
        mi.skip = self.pending_eob == 0;
        if mi.skip {
            coef_bits = 0;
        } else if !keep_recon {
            self.restore_block(mi_row, mi_col, bwl, bhl, &start);
        }
        (mi, coef_bits, sse)
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
        // Sub-8×8 blocks carry per-4×4 MVs (`bmi_mv`) and MC each sub-block separately.
        if mi.is_inter {
            if (mi.sb_type as usize) < BLOCK_8X8 {
                self.inter_predict_sub8x8(plane, mi, mi_row, mi_col, bsize, bwl, bhl);
            } else if mi.has_second_ref() {
                // Compound: ref 0 writes the prediction, ref 1 blends `(p0+p1+1)>>1`.
                let save = self.active_ref;
                self.active_ref = (mi.ref_frame[0] - 1) as usize;
                self.inter_predict_mv(plane, mi_row, mi_col, bsize, bwl, bhl, mi.mv[0], false);
                self.active_ref = (mi.ref_frame[1] - 1) as usize;
                self.inter_predict_mv(plane, mi_row, mi_col, bsize, bwl, bhl, mi.mv[1], true);
                self.active_ref = save;
            } else {
                self.inter_predict_mv(plane, mi_row, mi_col, bsize, bwl, bhl, mi.mv[0], false);
            }
        }

        let mut bits = 0u64;
        let mut sse = 0u64;
        // Abort bound applies only to costing trials (never the emit pass) and
        // only on luma (rd_cost_y's plane); chroma/skip trials need full sums.
        let abort_at = if enc.is_none() && plane == 0 {
            self.trial_abort_at
        } else {
            None
        };
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
                if let Some(bound) = abort_at {
                    if sse as f64 + self.lambda * (bits as f64 / 256.0) > bound {
                        return (u64::MAX, u64::MAX);
                    }
                }
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
            let _s = prof::Scope::new(prof::S::IntraPred);
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

        // Per-block working buffers. These are moved out of `self` rather than
        // declared as locals: as `[0i32; 1024]` stack arrays they cost a ~17 KB
        // zero-init on every one of the ~2.1M calls a 20-frame CIF encode makes,
        // which measured as the largest single unattributed bucket in the
        // encoder. Every buffer is fully written over `[..n]` before it is read,
        // so carrying stale bytes past `n` is unobservable. `mem::take` (a
        // pointer move on a `Box`) keeps the borrow checker happy alongside the
        // `&self.src` / `&mut self.rec` borrows below; it is restored at BOTH
        // exits. `encode_tx_block` is not re-entrant, so the box is never empty.
        let mut scratch = std::mem::take(&mut self.tx_scratch);
        if self.tx_memset {
            scratch.clear(); // VP9_TX_MEMSET=1 — the A/B oracle arm
        }
        let TxScratch {
            residual,
            coeffs,
            levels,
            dqcoeff,
            token_cache,
        } = &mut *scratch;

        let n = bs * bs;
        let src = &self.src[plane];
        // Row-sliced rather than 2-D indexed: the old form recomputed
        // `(y0+y)*src.stride + x0+x` per pixel and defeated vectorisation across
        // two different strides. Bounds are checked once per row, and the inner
        // loop is a flat u16->i32 subtract that LLVM can widen.
        for y in 0..bs {
            let s_row = &src.buf[(y0 + y) * src.stride + x0..][..bs];
            let p_row = &self.rec[plane].buf[dst_off + y * stride..][..bs];
            let d_row = &mut residual[y * bs..][..bs];
            for x in 0..bs {
                d_row[x] = s_row[x] as i32 - p_row[x] as i32;
            }
        }
        // Prediction-only SSE (Σ(src−pred)² = Σ residual²) — the distortion if this
        // block were coded `skip`. Accumulated on the measuring trial only (the
        // force_skip re-run re-uses the captured value). Same pixels the recon SSE
        // below sums, so the two are directly RD-comparable.
        if self.skip_trial && !self.force_skip {
            let mut psse = 0u64;
            for &r in &residual[..n] {
                psse += (r as i64 * r as i64) as u64;
            }
            self.pending_pred_sse += psse;
        }

        // ---- forward transform + quantize (inter / lossless / chroma / 32×32
        // are always DCT_DCT; only ≤16×16 intra luma uses the hybrid transform) ----
        let tx_type = if mi.is_inter || self.frame_lossless() || plane != 0 || tx_size == 3 {
            TxType::DctDct
        } else {
            INTRA_MODE_TO_TX_TYPE[mi.mode as usize]
        };
        // Residual fingerprint for the emit-dedup cache (FNV over the pipeline's
        // pure-function input; tx_type is implied by the key's plane+size+mi path).
        let is_emit = enc.is_some();
        // The pipeline output = f(residual, tx_type, quant params[plane/size],
        // trellis probs[inter], trellis entry ctx0) — ALL must be in the
        // fingerprint or the reuse silently drifts (measured: ctx0/inter leak).
        let act = self.above_ctx[plane][above_col0 + col..above_col0 + col + txw]
            .iter()
            .any(|&v| v != 0) as usize;
        let lct = self.left_ctx[plane][left_row0 + row..left_row0 + row + txw]
            .iter()
            .any(|&v| v != 0) as usize;
        let ctx0 = act + lct;
        let pt = plane.min(1);
        let inter = mi.is_inter as usize;
        let dedup_active = self.emit_dedup && (is_emit || self.skip_trial) && !self.force_skip;
        let dedup_hash = if dedup_active {
            let mut h = 0xcbf29ce484222325u64
                ^ (tx_type as u64)
                ^ ((ctx0 as u64) << 8)
                ^ ((inter as u64) << 16);
            for &r in &residual[..n] {
                h ^= r as u32 as u64;
                h = h.wrapping_mul(0x100000001b3);
            }
            h
        } else {
            0
        };
        let (scan, nb) = get_scan(tx_size, tx_type);
        let dq = if plane == 0 { self.dq_y } else { self.dq_uv };
        let dq_shift = if tx_size == 3 { 1 } else { 0 };
        // Emit-dedup reuse: on a residual-hash match the trial's outputs ARE this
        // block's outputs (pure function) — skip fwd+quantize+trellis entirely.
        let mut from_cache = false;
        let mut eob = 0usize;
        if dedup_active && is_emit {
            if let Some((h, lv, dqv, e)) =
                self.dedup_map.borrow().get(&(plane as u8, x0 as u32, y0 as u32, tx_size as u8))
            {
                if *h == dedup_hash {
                    levels[..n].copy_from_slice(lv);
                    dqcoeff[..n].copy_from_slice(dqv);
                    eob = *e as usize;
                    from_cache = true;
                }
            }
        }
        if !from_cache {
            {
                let _s = prof::Scope::new(prof::S::FwdTx);
                forward_transform(&residual[..n], bs, tx_type, &mut coeffs[..n]);
            }
            eob = {
                let _s = prof::Scope::new(prof::S::Quantize);
                quantize(
                    &coeffs[..n],
                    scan,
                    dq.0,
                    dq.1,
                    dq.1 as i64 * self.ac_round_num / 8,
                    dq_shift,
                    &mut levels[..n],
                    &mut dqcoeff[..n],
                )
            };
        }

        // force_skip: re-materialise this block as empty (RD skip decision dropped
        // the residual). eob=0 ⇒ no residual added below, context set to 0, recon ==
        // MC prediction — bit-identical to a naturally-empty (skip) block.
        if self.force_skip {
            eob = 0;
        }

        // ---- entropy context (ctx0/pt/inter computed above, pre-dedup) ----
        // Cost/trial probs = the frame's INITIAL context (== the spec defaults
        // when not chaining; the adapted context when chaining) so the RD cost
        // model matches what the emit actually pays. Static defaults here made
        // chained decisions systematically wrong (cost said expensive, emit was
        // cheap -> over-skipping, -2.3dB on akiyo).
        let default_probs = &self.fc.coef_probs[tx_size][pt][inter];
        // R5: trellis-style RD-optimal EOB on the commit path (uses the default
        // probs so the levels reproduce identically across the R4 two-pass).
        let mut trellis_rate: Option<u64> = None;
        // Trellis runs in emits AND exploration skip-trials (default): removing
        // it from trials (libvpx-style) was REFUTED at +21.5% BD — our RD-skip
        // gate compares j_noskip built from these coef bits, and non-trellised
        // bits inflate it into systematic over-skipping.
        let trellis_here =
            !from_cache && (enc.is_some() || (self.skip_trial && self.trellis_trials));
        // libvpx `trellis_opt_tx_rd` RESIDUAL_MSE gate (vp9_encoder.h
        // do_trellis_opt): run the trellis only when the residual energy is
        // small relative to the quantizer — `SSE ≤ npix·qstep²·thresh`,
        // qstep = ac_dequant>>3, their cpu-used-2 thresh = 3.0. DETERMINISTIC on
        // block data ⇒ identical decision in trial and emit ⇒ the RD-skip
        // coupling stays consistent (unlike the refuted asymmetric fast-trials).
        // `VP9_TRELLIS_MSE_T=0` disables the gate (always-trellis oracle).
        // NOTE: dropping trellis on the realtime tier was TRIED + REVERTED — only ~1.06×
        // (freed time shifts into coding more surviving coefficients) for +6.87% BD-rate;
        // trellis is load-bearing for our quality (libvpx compensates elsewhere in realtime).
        let trellis_gated = trellis_here && self.use_trellis && eob > 0 && {
            if self.trellis_mse_t > 0.0 {
                let mut rss = 0i64;
                for &r in &residual[..n] {
                    rss += (r as i64) * (r as i64);
                }
                let qstep = (dq.1 >> 3).max(1) as i64;
                (rss as f64) <= (n as f64) * ((qstep * qstep) as f64) * self.trellis_mse_t
            } else {
                true
            }
        };
        if trellis_gated {
            let _s = prof::Scope::new(prof::S::Trellis);
            let (new_eob, rate) = self.trellis_eob(
                &mut levels[..n],
                &mut dqcoeff[..n],
                &coeffs[..n],
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
                dq.0 as i64,
                dq.1 as i64,
                dq_shift as u32,
            );
            eob = new_eob;
            trellis_rate = rate;
        }
        // Emit-dedup store: the skip trial's final (levels, dqcoeff, eob) become
        // the cache entry for this tx block, keyed by the residual hash.
        if self.emit_dedup && self.skip_trial && !self.force_skip {
            self.dedup_map.borrow_mut().insert(
                (plane as u8, x0 as u32, y0 as u32, tx_size as u8),
                (
                    dedup_hash,
                    levels[..n].to_vec(),
                    dqcoeff[..n].to_vec(),
                    eob as u16,
                ),
            );
        }
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
        } else if let Some(rate) = trellis_rate {
            // The trellis already tracked the exact rate of the final block —
            // identical to the coef_cost walk below, so don't re-walk.
            rate
        } else {
            // RDO trial: cost the exact same token walk (default probs) without emitting.
            let _s = prof::Scope::new(prof::S::CoefCost);
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
        let _s = prof::Scope::new(prof::S::CtxUpdate);
        let inframe_w = (max_w - col).min(txw);
        let inframe_h = (max_h - row).min(txw);
        let v = (eob > 0) as u8;
        for i in 0..txw {
            self.above_ctx[plane][above_col0 + col + i] = if i < inframe_w { v } else { 0 };
            self.left_ctx[plane][left_row0 + row + i] = if i < inframe_h { v } else { 0 };
        }
        drop(_s);
        self.pending_eob += eob as u32;

        // skip_recode analog (libvpx SF): INTER mode-trials never read the recon
        // (block-level MC prediction; restore_y follows immediately), so skip the
        // inverse transform + pixel-SSE and estimate the distortion in the
        // COEFFICIENT domain by Parseval — the same model the trellis uses
        // (measured −0.50% BD there). Intra trials keep the real recon (it feeds
        // the next tx block's prediction edges); emits/skip-trials unchanged.
        // `VP9_TRIAL_RECON=1` restores the exact path.
        if !is_emit && !self.skip_trial && mi.is_inter && !self.trial_recon {
            let norm = basis_normsq(tx_size, tx_type);
            let mut d = 0.0f64;
            for p in 0..n {
                let e = (dqcoeff[p] - coeffs[p]) as f64;
                d += e * e * norm[p];
            }
            self.tx_scratch = scratch; // restore before the early exit
            return (bits, d as u64);
        }

        // ---- reconstruct: add the dequantized residual back ----
        if eob > 0 {
            let _s = prof::Scope::new(prof::S::InvTxRecon);
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
        // Row-sliced for the same reason as the residual loop above: one bounds
        // check per row instead of two per pixel, and an inner loop LLVM can
        // widen. The i32 product cannot overflow: pixels are <= 16-bit, so the
        // squared difference is < 2^32 and 1024 of them still fit in u64.
        let src = &self.src[plane];
        let rec = &self.rec[plane].buf;
        let mut sse = 0u64;
        for y in 0..bs {
            let s_row = &src.buf[(y0 + y) * src.stride + x0..][..bs];
            let r_row = &rec[dst_off + y * stride..][..bs];
            let mut acc = 0u64;
            for x in 0..bs {
                let d = s_row[x] as i64 - r_row[x] as i64;
                acc += (d * d) as u64;
            }
            sse += acc;
        }
        self.tx_scratch = scratch;
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
        coeffs: &[i32],
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
        dc_step: i64,
        ac_step: i64,
        dq_shift: u32,
    ) -> (usize, Option<u64>) {
        let n = bs * bs;
        // COEFFICIENT-DOMAIN distortion (parity with libvpx `optimize_b`): the pixel
        // SSE of dequantized coefficients equals, by Parseval on the orthogonal
        // DCT/ADST basis, Σ_p (dqcoeff[p] − coeff[p])²·norm[p] where `coeff` is the
        // un-quantized forward transform and norm[p] is position p's basis energy —
        // so we NEVER run an inverse transform per candidate (the 31×-slower path).
        // It's an approximation (integer-idct rounding + clamp break exactness), so
        // `VP9_TRELLIS_EXACT=1` restores the pixel-SSE oracle for A/B.
        // Content-adaptive λ: DENSE blocks (high eob/n = noisy high-motion residual) trim
        // more aggressively; SPARSE blocks (static detail) stay near self.lambda.
        let frac = eob as f64 / (bs * bs) as f64;
        let lambda = self.lambda * self.trellis_lambda_scale * (1.0 + self.trellis_k * frac);
        // ---- Exact pixel-SSE oracle (VP9_TRELLIS_EXACT): inverse-transform + SSE per
        // candidate — the slow 31× path, kept only for A/B against the fast one. ----
        if self.trellis_exact {
            let mut pred = [0u16; 1024];
            for y in 0..bs {
                for x in 0..bs {
                    pred[y * bs + x] = self.rec[plane].buf[dst_off + y * stride + x];
                }
            }
            let src = &self.src[plane];
            let mut tc = [0u8; 1024];
            let mut rd = |dq: &[i32], lv: &[i32], e: usize| -> f64 {
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
                        &dq[..n], bs, tx_type, &mut temp[..n], bs, self.max_px, max_row + 1,
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
                let r = coef_cost(&lv[..n], scan, nb, e, probs, tx_size, ctx0, &mut tc[..n], 8);
                d as f64 + lambda * (r as f64 / 256.0)
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
            let mut i = eob;
            while i > 0 {
                i -= 1;
                let pos = scan[i] as usize;
                let lv = levels[pos];
                if lv == 0 {
                    continue;
                }
                let step = if pos == 0 { dc_step } else { ac_step };
                let sign = if lv < 0 { -1i32 } else { 1 };
                let mag = lv.unsigned_abs() as i64 - 1;
                let (ol, od) = (levels[pos], dqcoeff[pos]);
                levels[pos] = sign * mag as i32;
                dqcoeff[pos] = sign * ((mag * step) >> dq_shift) as i32;
                let mut ne = eob;
                while ne > 0 && levels[scan[ne - 1] as usize] == 0 {
                    ne -= 1;
                }
                let jp = rd(dqcoeff, levels, ne);
                if jp < j {
                    j = jp;
                    eob = ne;
                } else {
                    levels[pos] = ol;
                    dqcoeff[pos] = od;
                }
            }
            return (eob, None);
        }
        // ---- Fast coefficient-domain trellis (default): distortion maintained
        // INCREMENTALLY (O(1)/candidate via Parseval); only the rate is re-costed. ----
        let norm = basis_normsq(tx_size, tx_type);
        let dist_of = |p: usize, dqv: i32| -> f64 {
            let e = (dqv - coeffs[p]) as f64;
            e * e * norm[p]
        };
        // Distortion is tracked RELATIVE to the initial block (baseline 0): every
        // decision is a `jp < j` comparison, so the absolute Σ(dq−coeff)²·norm
        // baseline cancels and need never be summed — dropping an O(bs²) per-call loop.
        let mut dist = 0.0f64;
        // Rate maintained INCREMENTALLY too (parity with libvpx `optimize_b`): the
        // O(1)-per-candidate `RateTracker` replaces the O(eob) `coef_cost` re-walk —
        // its `total()` is bit-for-bit the same rate the re-walk produced, so the
        // trellis decisions are byte-identical, only the whole loop is now O(eob).
        // Its per-position scratch is thread-local + reused so building a tracker per
        // block costs no allocation.
        thread_local! {
            static TR_SCRATCH: std::cell::RefCell<(Vec<u8>, Vec<u64>, Vec<u8>)> =
                std::cell::RefCell::new((vec![0u8; 1024], vec![0u64; 1025], vec![0u8; 1024]));
        }
        TR_SCRATCH.with(|s| {
        let mut g = s.borrow_mut();
        let (cbuf, pbuf, bbuf) = &mut *g;
        let mut tr = RateTracker::new(
            levels, scan, nb, eob, probs, tx_size, tx_type as u8, ctx0, 8, cbuf, pbuf, bbuf,
        );
        let rj = |q8: u64| lambda * (q8 as f64 / 256.0);
        let mut eob = eob;
        let mut j = dist + rj(tr.total());
        // EOB trim.
        while eob > 0 {
            let last = scan[eob - 1] as usize;
            let (sl, sd) = (levels[last], dqcoeff[last]);
            let d_new = dist - dist_of(last, sd) + dist_of(last, 0);
            levels[last] = 0;
            dqcoeff[last] = 0;
            let mut ne = eob - 1;
            while ne > 0 && levels[scan[ne - 1] as usize] == 0 {
                ne -= 1;
            }
            let jp = d_new + rj(tr.probe(levels, eob - 1, ne));
            if jp < j {
                j = jp;
                dist = d_new;
                tr.commit(levels, eob - 1, ne);
                eob = ne;
            } else {
                levels[last] = sl;
                dqcoeff[last] = sd;
                break;
            }
        }
        // Interior lowering. DP-lite (default): pure magnitude lowerings
        // (|lv| ≥ 2, not the last nonzero) price the rate delta with the FROZEN
        // build-time (band, ctx) — a single table difference, no neighbour
        // updates — exactly libvpx optimize_b's frozen-context approximation.
        // Status-changing candidates (1→0, tail) keep the exact tracker path.
        // `VP9_TRELLIS_EXACT_CTX=1` restores exact pricing for everything.
        let frozen = self.trellis_frozen;
        let mut rate_adj = 0i64; // Σ accepted frozen deltas (Q8)
        let mut i = eob;
        while i > 0 {
            i -= 1;
            let pos = scan[i] as usize;
            let lv = levels[pos];
            if lv == 0 {
                continue;
            }
            let step = if pos == 0 { dc_step } else { ac_step };
            let sign = if lv < 0 { -1i32 } else { 1 };
            let aval = lv.unsigned_abs();
            let mag = aval as i64 - 1;
            let (ol, od) = (levels[pos], dqcoeff[pos]);
            let new_dq = sign * ((mag * step) >> dq_shift) as i32;
            let d_new = dist - dist_of(pos, od) + dist_of(pos, new_dq);
            if frozen && aval >= 2 && i + 1 != eob {
                let (band, fctx) = tr.frozen(i);
                let p2 = tr.probs()[band][fctx][2];
                if let (Some(cn), Some(co)) = (
                    crate::encode::tokens::mag_cost_q8(p2, aval - 1),
                    crate::encode::tokens::mag_cost_q8(p2, aval),
                ) {
                    let delta = cn as i64 - co as i64;
                    let jp = d_new + lambda * ((rate_adj + delta) as f64 / 256.0)
                        + rj(tr.total());
                    if jp < j {
                        levels[pos] = sign * mag as i32;
                        dqcoeff[pos] = new_dq;
                        dist = d_new;
                        rate_adj += delta;
                        j = jp;
                    }
                    continue;
                }
            }
            levels[pos] = sign * mag as i32;
            dqcoeff[pos] = new_dq;
            let mut ne = eob;
            while ne > 0 && levels[scan[ne - 1] as usize] == 0 {
                ne -= 1;
            }
            let jp = d_new + lambda * (rate_adj as f64 / 256.0) + rj(tr.probe(levels, i, ne));
            if jp < j {
                j = jp;
                dist = d_new;
                tr.commit(levels, i, ne);
                eob = ne;
            } else {
                levels[pos] = ol;
                dqcoeff[pos] = od;
            }
        }
        // The tracker total plus the frozen-delta adjustment approximates the
        // final rate (exact when DP-lite is off — then rate_adj == 0 and this is
        // bit-for-bit coef_cost, per `rate_tracker_matches_coef_cost_incrementally`).
        (eob, Some((tr.total() as i64 + rate_adj).max(0) as u64))
        })
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
    /// Luma SSE of the reconstruction deblocked at `lvl` (into scratch `c0`), for the R3
    /// level search. Filters ONLY luma — the level choice is luma-SSE-gated, so cloning +
    /// deblocking chroma per candidate would be discarded work; the winner's real 3-plane
    /// filter is applied by the caller.
    fn lf_luma_sse(&self, h: &mut FrameHeader, c0: &mut [u16], lvl: u32) -> u64 {
        h.loop_filter_level = lvl;
        c0.copy_from_slice(&self.rec[0].buf);
        let mut planes = [(c0, self.rec[0].stride, 0usize, 0usize)];
        loop_filter_frame(&mut planes, &self.mi, self.mi_rows, self.mi_cols, h);
        self.luma_sse_of(planes[0].0)
    }

    /// Observe-only per-segment-lf ceiling probe: the per-64×64-SB oracle (each SB filtered at
    /// its own best level) vs the global-best-level SSE. Bounds the spatial per-segment win.
    fn lfseg_probe(&self, h: &mut FrameHeader, global_sse: u64) {
        use std::sync::atomic::Ordering::Relaxed;
        let levels: [u32; 13] = [0, 4, 8, 12, 16, 20, 24, 28, 32, 40, 48, 56, 63];
        let src = &self.src[0];
        let (w, hgt, stride) = (src.w, src.h, src.stride);
        let (sb_cols, sb_rows) = (w.div_ceil(64), hgt.div_ceil(64));
        let mut sb_min = vec![u64::MAX; sb_cols * sb_rows];
        let mut c0 = self.rec[0].buf.clone();
        for &lvl in &levels {
            let filtered: &[u16] = if lvl == 0 {
                &self.rec[0].buf
            } else {
                h.loop_filter_level = lvl;
                c0.copy_from_slice(&self.rec[0].buf);
                let mut planes = [(&mut c0[..], self.rec[0].stride, 0usize, 0usize)];
                loop_filter_frame(&mut planes, &self.mi, self.mi_rows, self.mi_cols, h);
                &c0
            };
            for sr in 0..sb_rows {
                let (y0, y1) = (sr * 64, ((sr + 1) * 64).min(hgt));
                for sc in 0..sb_cols {
                    let (x0, x1) = (sc * 64, ((sc + 1) * 64).min(w));
                    let mut sse = 0u64;
                    for y in y0..y1 {
                        let row = y * stride;
                        for x in x0..x1 {
                            let d = src.buf[row + x] as i64 - filtered[row + x] as i64;
                            sse += (d * d) as u64;
                        }
                    }
                    let sb = sr * sb_cols + sc;
                    sb_min[sb] = sb_min[sb].min(sse);
                }
            }
        }
        let ceiling: u64 = sb_min.iter().sum();
        LFSEG_PROBE[0].fetch_add(global_sse, Relaxed);
        LFSEG_PROBE[1].fetch_add(ceiling, Relaxed);
        LFSEG_PROBE[2].fetch_add(1, Relaxed);
    }

    fn apply_loop_filter(&mut self, h: &mut FrameHeader) {
        if self.disable_lf {
            h.loop_filter_level = 0;
            self.lf_level = 0;
            return;
        }
        // The level costs a fixed 6 header bits regardless of value, so a finer/wider search
        // is a MONOTONIC recon-SSE improvement (same rate, lower distortion) — it can only
        // help BD. Level 0 (no filter) is the baseline; a step-8 coarse sweep across the FULL
        // 0..=63 range (the old search capped at 32 + step 8 missed the fine optimum and the
        // heavy-filter regime low-bitrate frames want), then a ±4/±2/±1 descent to the exact
        // per-frame optimum. `VP9_LF_COARSE` restores the old {8,16,24,32} search (A/B oracle).
        // FAST PATH (`VP9_LF_FROM_Q=1`): predict the level from the AC quantizer
        // instead of searching, à la libvpx's LPF_PICK_FROM_Q. The constants are
        // OUR OWN least-squares refit (libvpx's were fitted to libvpx's encoder),
        // harvested over 1440 frames × 4 clips × 3 QPs:
        //   KEY   level = 0.11137·q + 1.068   (MAE 1.51, R² 0.994)
        //   INTER level = 0.06849·q + 6.956   (MAE 4.17, R² 0.845)
        // Fixed-point at 2^18 to keep the encoder integer-only.
        if self.lf_from_q {
            let q = crate::quant::ac_quant(h.base_q_idx as i32, 8);
            let lvl = if h.key_frame || h.intra_only {
                (q * 29193 + 279970 + (1 << 17)) >> 18
            } else {
                (q * 17952 + 1823274 + (1 << 17)) >> 18
            }
            .clamp(0, 63) as u32;
            h.loop_filter_level = lvl;
            self.lf_level = lvl;
            if lvl > 0 {
                let [p0, p1, p2] = &mut self.rec;
                let mut planes = [
                    (&mut p0.buf[..], p0.stride, p0.ss_x, p0.ss_y),
                    (&mut p1.buf[..], p1.stride, p1.ss_x, p1.ss_y),
                    (&mut p2.buf[..], p2.stride, p2.ss_x, p2.ss_y),
                ];
                loop_filter_frame(&mut planes, &self.mi, self.mi_rows, self.mi_cols, h);
            }
            return;
        }
        let mut best = (0u32, self.luma_sse_of(&self.rec[0].buf));
        let mut c0 = self.rec[0].buf.clone();
        let coarse: &[u32] = if std::env::var("VP9_LF_COARSE").is_ok() {
            &[8, 16, 24, 32]
        } else {
            &[8, 16, 24, 32, 40, 48, 56, 63]
        };
        for &lvl in coarse {
            let sse = self.lf_luma_sse(h, &mut c0, lvl);
            if sse < best.1 {
                best = (lvl, sse);
            }
        }
        if std::env::var("VP9_LF_COARSE").is_err() {
            // Local descent around the coarse best (re-centres each step → bridges the
            // step-8 gaps to the true minimum).
            for step in [4u32, 2, 1] {
                for cand in [best.0.saturating_sub(step), (best.0 + step).min(63)] {
                    if cand == 0 || cand == best.0 {
                        continue;
                    }
                    let sse = self.lf_luma_sse(h, &mut c0, cand);
                    if sse < best.1 {
                        best = (cand, sse);
                    }
                }
            }
        }
        // (Sharpness search REMOVED: measured a proven no-op — sharpness=0 is always the
        // SSE-optimal choice, since max deblocking minimises blocking error vs source and any
        // sharpness>0 only reduces filtering ⇒ higher SSE. +0.00% BD on all clips, cost only.)
        if self.lfseg_probe_on {
            self.lfseg_probe(h, best.1);
        }
        // (Mode/ref + per-segment loop-filter DELTAS were tested and PRUNED — the per-SB
        // oracle ceiling is only ~0.26–0.49% luma SSE, and an SSE-searched mode/ref-delta
        // pass LOST BD on every clip (+0.35..+0.94%): the tiny luma gain doesn't cover the
        // header signalling AND the luma-only search over-filters chroma. The global level
        // search above already captures the loop-filter BD. See `lfseg_probe`.)
        // HARVEST TAP (observe-only, `VP9_LFHARVEST=1`): the searched optimum
        // alongside the features a predictor could use. Feeds the offline
        // question "can a formula replace this 14-evaluation search?".
        if std::env::var_os("VP9_LFHARVEST").is_some() {
            let q = crate::quant::ac_quant(h.base_q_idx as i32, 8);
            let key = h.key_frame || h.intra_only;
            // libvpx's LPF_PICK_FROM_Q closed form, for comparison.
            let guess = if key {
                ((q * 17563 - 421574) + (1 << 17)) >> 18
            } else {
                ((q * 20723 + 1015158) + (1 << 17)) >> 18
            }
            .clamp(0, 63);
            let sse_guess = self.lf_luma_sse(h, &mut c0, guess as u32);
            eprintln!(
                "LFHARVEST	q={}	qidx={}	key={}	best={}	sse_best={}	guess={}	sse_guess={}	sse_lvl0={}",
                q, h.base_q_idx, key as u8, best.0, best.1, guess, sse_guess,
                self.luma_sse_of(&self.rec[0].buf)
            );
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

/// Decoder's `tile_offset`, mirrored: mi-col start of tile `idx`.
/// Per-position basis energy `norm[r·bs+c] = norm_col[r]·norm_row[c] / 2^(2·shift)`
/// for the size/type's separable inverse transform — the weights that turn a
/// coefficient-domain error into a pixel-SSE estimate (the encoder's fast trellis
/// distortion). Cached per (tx_size, tx_type); built once from the 1-D basis norms.
fn basis_normsq(tx_size: usize, tx_type: TxType) -> std::rc::Rc<[f64; 1024]> {
    thread_local! {
        // 16 (tx_size × tx_type) combos — flat array, no hashing on the hot path.
        static CACHE: std::cell::RefCell<[Option<std::rc::Rc<[f64; 1024]>>; 16]> =
            const { std::cell::RefCell::new([const { None }; 16]) };
    }
    let key = tx_size * 4 + tx_type as usize;
    if let Some(v) = CACHE.with(|c| c.borrow()[key].clone()) {
        return v;
    }
    let bs = 4usize << tx_size;
    let shift = match bs {
        4 => 4,
        8 => 5,
        _ => 6,
    };
    // inverse_transform_add_rows: (row_adst, col_adst) per tx_type.
    let (row_adst, col_adst) = match tx_type {
        TxType::DctDct => (false, false),
        TxType::AdstDct => (false, true),
        TxType::DctAdst => (true, false),
        TxType::AdstAdst => (true, true),
    };
    let norm_row = inv_basis_normsq_1d(bs, row_adst);
    let norm_col = inv_basis_normsq_1d(bs, col_adst);
    let scale = 1.0f64 / (1u64 << (2 * shift)) as f64;
    let mut norm = [0.0f64; 1024];
    for r in 0..bs {
        for c in 0..bs {
            norm[r * bs + c] = norm_col[r] * norm_row[c] * scale;
        }
    }
    let rc = std::rc::Rc::new(norm);
    CACHE.with(|cache| cache.borrow_mut()[key] = Some(rc.clone()));
    rc
}

fn tile_offset_enc(idx: usize, mi_cols: usize, log2: u32) -> usize {
    let sb_cols = mi_cols.div_ceil(8);
    (((idx * sb_cols) >> log2) << 3).min(mi_cols)
}

// ---------------------------------------------------------------------------
// SAD kernel — the encoder's hottest primitive (integer motion search, ~half of
// encode time). The scalar version is the oracle + the non-AVX2 fallback; the
// AVX2 twin is bit-identical (integer ops only). See `codec-vectorize-kernel`.
// ---------------------------------------------------------------------------

/// Σ|src − ref| over an 8×8 block. `s`/`r` start at the block top-left; `ss`/`rs`
/// are element strides. Scalar oracle. Values are 8-bit-in-`u16`, so the sum
/// (≤ 64·65535) fits `u32`.
#[inline]
fn sad8x8_scalar(s: &[u16], ss: usize, r: &[u16], rs: usize) -> u32 {
    let mut sad = 0u32;
    for y in 0..8 {
        let (so, ro) = (y * ss, y * rs);
        for x in 0..8 {
            sad += (s[so + x] as i32 - r[ro + x] as i32).unsigned_abs();
        }
    }
    sad
}

/// AVX2 twin of [`sad8x8_scalar`], bit-identical. Two rows (16×`u16`) per
/// iteration: `|a−b|` via a pair of saturating subtracts OR'd together, then
/// `madd_epi16` with 1s horizontally sums each lane into `i32` accumulators
/// (valid because every `|a−b| ≤ 255` here, well inside `i16`).
///
/// # Safety
/// Requires AVX2. The caller passes an in-bounds interior window, so `s`/`r`
/// have at least `7·stride + 8` elements (each `loadu` reads 8 `u16`).
#[cfg(target_arch = "x86_64")]
#[target_feature(enable = "avx2")]
unsafe fn sad8x8_avx2(s: &[u16], ss: usize, r: &[u16], rs: usize) -> u32 {
    use std::arch::x86_64::*;
    let (sp, rp) = (s.as_ptr(), r.as_ptr());
    let ones = _mm256_set1_epi16(1);
    let mut acc = _mm256_setzero_si256();
    let mut y = 0usize;
    while y < 8 {
        let s0 = _mm_loadu_si128(sp.add(y * ss) as *const __m128i);
        let s1 = _mm_loadu_si128(sp.add((y + 1) * ss) as *const __m128i);
        let sa = _mm256_set_m128i(s1, s0);
        let r0 = _mm_loadu_si128(rp.add(y * rs) as *const __m128i);
        let r1 = _mm_loadu_si128(rp.add((y + 1) * rs) as *const __m128i);
        let ra = _mm256_set_m128i(r1, r0);
        let d = _mm256_or_si256(_mm256_subs_epu16(sa, ra), _mm256_subs_epu16(ra, sa));
        acc = _mm256_add_epi32(acc, _mm256_madd_epi16(d, ones));
        y += 2;
    }
    // Horizontal sum of the 8 i32 lanes.
    let lo = _mm256_castsi256_si128(acc);
    let hi = _mm256_extracti128_si256::<1>(acc);
    let mut t = _mm_add_epi32(lo, hi);
    t = _mm_add_epi32(t, _mm_shuffle_epi32::<0b11_10_11_10>(t));
    t = _mm_add_epi32(t, _mm_shuffle_epi32::<0b01_01_01_01>(t));
    _mm_cvtsi128_si32(t) as u32
}

/// Dispatch: AVX2 when available (flag cached by the caller), else scalar.
#[inline]
fn sad8x8(s: &[u16], ss: usize, r: &[u16], rs: usize, has_avx2: bool) -> u32 {
    #[cfg(target_arch = "x86_64")]
    if has_avx2 {
        // SAFETY: `has_avx2` proves the target feature; the window is in-bounds.
        return unsafe { sad8x8_avx2(s, ss, r, rs) };
    }
    let _ = has_avx2;
    sad8x8_scalar(s, ss, r, rs)
}

/// Σ|src − ref| over a 4×4 block (sub-8×8 motion search). Scalar oracle.
#[inline]
fn sad4x4_scalar(s: &[u16], ss: usize, r: &[u16], rs: usize) -> u32 {
    let mut sad = 0u32;
    for y in 0..4 {
        let (so, ro) = (y * ss, y * rs);
        for x in 0..4 {
            sad += (s[so + x] as i32 - r[ro + x] as i32).unsigned_abs();
        }
    }
    sad
}

// NOTE: an AVX2 4×4 SAD was built + proven bit-identical but measured SLOWER than
// `sad4x4_scalar` (16 elements can't amortise the horizontal reduction) — reverted.
// The scalar branchless kernel above is the one used.

#[cfg(test)]
mod tests {
    use super::*;
    use rff_codec::Decoder;
    use rff_core::{CodecId, Frame, Packet};

    /// The AVX2 SAD kernel must be bit-identical to the scalar oracle over many
    /// random strided 8-bit blocks (the codec is 8-bit, values 0..=255).
    #[test]
    fn sad8x8_avx2_matches_scalar() {
        #[cfg(target_arch = "x86_64")]
        {
            if !std::is_x86_feature_detected!("avx2") {
                return; // non-AVX2 CI host: scalar is the only path anyway.
            }
            let mut seed = 0x1234_5678_9abc_def0u64;
            let mut xr = || {
                seed ^= seed << 13;
                seed ^= seed >> 7;
                seed ^= seed << 17;
                seed
            };
            for _ in 0..4000 {
                let ss = 8 + (xr() % 40) as usize;
                let rs = 8 + (xr() % 40) as usize;
                let mut s = vec![0u16; ss * 8];
                let mut r = vec![0u16; rs * 8];
                for v in s.iter_mut() {
                    *v = (xr() % 256) as u16;
                }
                for v in r.iter_mut() {
                    *v = (xr() % 256) as u16;
                }
                let a = sad8x8_scalar(&s, ss, &r, rs);
                let b = unsafe { sad8x8_avx2(&s, ss, &r, rs) };
                assert_eq!(a, b, "8x8 mismatch ss={ss} rs={rs}");
            }
        }
    }

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

            // Forward prob update is now default-OFF (it inflates inter frames — see the
            // constructor note), so opt it back in explicitly: this test validates that
            // when ENABLED it still shrinks this dense synthetic keyframe corpus and
            // round-trips bit-exact (the regime where the feature is a genuine win).
            let mut on = FrameEncoder::new(w, h, 64, src, None);
            on.set_use_prob_updates(true);
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
        // Pin λ to the historical default: this test verifies the empty-block→skip
        // *conformance* machinery, not the shipped resolution-adaptive λ tuning (which
        // deliberately lowers λ, coding more residual so fewer blocks fall fully empty).
        let mut enc0 = FrameEncoder::new(w, h, 48, src(), None);
        enc0.set_lambda_mult(0.001);
        let key = enc0.encode_frame();
        let recon0 = enc0.recon_owned();
        // Same content ⇒ ZEROMV re-predicts it exactly ⇒ most blocks empty ⇒ skip.
        let mut enc1 = FrameEncoder::new(w, h, 48, src(), Some(recon0));
        enc1.set_lambda_mult(0.001);
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
