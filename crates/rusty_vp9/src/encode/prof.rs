//! Primitive profiler for the VP9 encoder hot path (`VP9_PROF2=1`, or
//! [`set_enabled`] from an in-process harness).
//!
//! `rdtsc` cycle timers keyed by a fixed set of stages, accumulated with per-stage
//! call counts into process-global atomics, dumped by [`dump`]. `rdtsc` (not
//! `Instant`) because the innermost kernels are sub-µs — `Instant::as_micros()`
//! truncates a 0.05 µs call to 0, which silently hid transform/quantize time in
//! the "glue" bucket. Every probe is a no-op unless `VP9_PROF2` is set. At dump we
//! measure the empty-scope `rdtsc` overhead and subtract `calls × overhead` per
//! stage so the millions-of-calls kernels aren't inflated by the instrument.
//!
//! Two ways to read the buckets. [`dump`] prints the human table at end of
//! encode; [`reset`] + [`snapshot`] give an in-process harness the same numbers
//! programmatically, which is how `video-tests/analyzer` takes a median over
//! repeated encodes. The stages are NESTED-inclusive (see [`parent`]) — unlike
//! the libvpx reference twin, whose stack-based profiler is exclusive by
//! construction — so the report subtracts children before comparing the two.

use std::sync::atomic::{AtomicU64, AtomicU8, Ordering::Relaxed};
use std::sync::OnceLock;
use std::time::Instant;

/// Hot-path stages we attribute time to. Keep in sync with `NAMES`.
/// Indented names = a subset that nests inside the parent above it.
#[derive(Clone, Copy)]
pub enum S {
    Total = 0,    // whole decision pass — the % denominator
    MotionSearch, // search_mv (SAD full-search) — aggregate (parent)
    IntSearch,    //   integer full-pel grid
    SubpelSearch, //   1/4-pel refinement
    Interp,       //     8-tap subpel interp inside refinement
    Mc,           // inter_predict_mv (8-tap subpel MC, commit)
    FwdTx,        // forward_transform kernel
    Quantize,     // quantize kernel
    CoefCost,     // coef_cost (RD bit costing)
    Trellis,      // trellis_eob (per-coef RD + inverse transforms)
    InvTxRecon,   // inverse transform + add (reconstruction)
    IntraPred,    // build_intra_edges + intra predict
    Sub8x8Search, // sub8x8_search_ref (per-4×4 SAD search) — nests NO other scope
    SnapRestore,  // GLUE: entropy-context snapshot/restore around RD trials
    CtxUpdate,    // GLUE: entropy-context maintenance (set_ctx, partition ctx)
    MvRefs,       // GLUE: MV reference prediction (find_mv_refs, NEAREST/NEAR)
    PredSse,      // shortlist prediction+SSE scoring (pred_sse — inline interp, own leaf)
    // --- INFO stages (NOT in TOPLEVEL; inclusive totals for decomposing orchestration).
    // self-time = the stage's total minus its scoped kernel children (computed by hand).
    Sub8x8,       // INFO: decide_sub8x8 — inclusive, nests the search AND a full residual trial
    RdCost,       // INFO: rd_cost_yuv + rd_cost_y (per-candidate RD; nests encode_plane kernels)
    DecideLeaf,   // INFO: decide_inter (whole leaf mode decision; nests motion/pred_sse/rd_cost)
    // --- Phase-0 glue bisection (2026-07-26). `rd_pick_partition` carries no scope,
    // so its whole body falls into the unscoped-orchestration residue (67.7 s =
    // 20.9% of encode vs libvpx's 3.3%). These four INFO buckets carve up that
    // body. They are deliberately NOT a scope on the function itself: it RECURSES,
    // and a nested-inclusive bucket would re-add the same span at every depth.
    ModeMap,      // INFO: mode_map HashMap probe/insert (SipHash on a dense coord key)
    SnapDrop,     // INFO: BlockSnap Drop — the free() half of snap_block's 7 Vecs
    PartCtx,      // INFO: partition_plane_context + part_flag_cost + G1 gate arithmetic
    VarTree,      // INFO: build_vt + variance (the content-dispatch tree)
    // Round 2 (2026-07-26): the first bisection left 65% of the glue dark. These
    // cover the remaining unscoped per-block bookkeeping in the decision path.
    StoreMi,      // INFO: store_mi — splat the winning ModeInfo across the block's mi grid
    MiCost,       // INFO: {intra,inter,sub8x8}_modeinfo_cost_q8 — mode-info bit pricing
    Count,
}

/// Number of stage buckets. Public so an out-of-process analyzer can size its
/// snapshot array without duplicating the enum.
pub const N: usize = S::Count as usize;
const NAMES: [&str; N] = [
    "TOTAL(decision)",
    "motion_search",
    "  int_search",
    "  subpel_search",
    "    interp_8tap",
    "mc_subpel",
    "fwd_tx",
    "quantize",
    "coef_cost",
    "trellis",
    "invtx_recon",
    "intra_pred",
    "  sub8x8_search",
    "snap_restore",
    "ctx_update",
    "mv_refs",
    "pred_sse",
    "[i]sub8x8",
    "[i]rd_cost",
    "[i]decide_leaf",
    "[i]mode_map",
    "[i]snap_drop",
    "[i]part_ctx",
    "[i]var_tree",
    "[i]store_mi",
    "[i]mi_cost",
];

// The disjoint, non-nested top-level stages (so glue = TOTAL − Σ these). Excludes
// the nested motion children (int/subpel/interp) and TOTAL itself.
//
// `Sub8x8` is deliberately NOT here. `Scope` is a plain inclusive RAII timer with
// no child subtraction, and `decide_sub8x8` runs a full residual trial —
// `snap_block` plus `encode_plane` — so its span already contains SnapRestore,
// FwdTx, Quantize, CoefCost, Trellis and InvTxRecon. Summing it alongside those
// double-counted them, which inflated the denominator and correspondingly
// UNDERSTATED the unscoped glue. It is reported as an inclusive `[i]` diagnostic
// instead, and its exclusive part — the per-4×4 SAD search — carries its own
// `Sub8x8Search` scope, which nests nothing and so is a legitimate partition member.
const TOPLEVEL: [S; 13] = [
    S::MotionSearch, S::Mc, S::FwdTx, S::Quantize, S::CoefCost, S::Trellis,
    S::InvTxRecon, S::IntraPred, S::Sub8x8Search, S::SnapRestore, S::CtxUpdate, S::MvRefs,
    S::PredSse,
];

static CYC: [AtomicU64; N] = [const { AtomicU64::new(0) }; N];
static COUNT: [AtomicU64; N] = [const { AtomicU64::new(0) }; N];

/// Enabled state. Seeded from `VP9_PROF2` on first touch, but also settable at
/// runtime by [`set_enabled`] so an in-process harness (the `video-tests`
/// analyzer) can turn the taps on for its stages pass without re-execing itself
/// with a different environment.
static ENABLED: AtomicU8 = AtomicU8::new(ENABLED_UNSET);
const ENABLED_UNSET: u8 = 2;

#[inline(always)]
fn enabled() -> bool {
    match ENABLED.load(Relaxed) {
        0 => false,
        1 => true,
        _ => {
            let on = std::env::var("VP9_PROF2").is_ok();
            ENABLED.store(on as u8, Relaxed);
            on
        }
    }
}

/// Turn the taps on or off for this process. The analyzer's `stages` pass calls
/// `set_enabled(true)`; the `speed` pass never does, so a profiler-off run pays
/// only one relaxed load per scope.
pub fn set_enabled(on: bool) {
    ENABLED.store(on as u8, Relaxed);
}

/// Zero every bucket. Paired with [`snapshot`] to measure one encode in
/// isolation and to take a median over repeated passes.
pub fn reset() {
    for i in 0..N {
        CYC[i].store(0, Relaxed);
        COUNT[i].store(0, Relaxed);
    }
}

/// Per-stage `(milliseconds, calls)` for the work recorded since the last
/// [`reset`], with the per-scope `rdtsc` self-overhead removed exactly as
/// [`dump`] does — so a programmatic reader and the printed table agree.
pub fn snapshot() -> [(f64, u64); N] {
    let (t0, c0) = *CAL.get_or_init(|| (Instant::now(), rdtsc()));
    let secs = t0.elapsed().as_secs_f64().max(1e-9);
    let hz = (rdtsc().wrapping_sub(c0)) as f64 / secs;
    let ovh_self = measure_scope_overhead().0;
    let mut out = [(0.0f64, 0u64); N];
    for (i, o) in out.iter_mut().enumerate() {
        let calls = COUNT[i].load(Relaxed);
        let cyc = CYC[i].load(Relaxed).saturating_sub(calls.saturating_mul(ovh_self));
        *o = (cyc as f64 / hz * 1e3, calls);
    }
    out
}

/// `(self_ns, full_ns)` — the measured cost of one scope on THIS machine.
///
/// * `self_ns` is already subtracted from every bucket by [`snapshot`].
/// * `full_ns` is the cost that leaks into the PARENT's span (2 `rdtsc` plus 2
///   atomic `fetch_add`). A reader that wants percentages of the raw decision
///   wall must remove `calls × full_ns` from it first: with tens of millions of
///   scope entries this tax is a real fraction of the wall, and folding it into
///   the glue bucket would invent orchestration work that does not exist.
pub fn overhead_ns() -> (f64, f64) {
    let (t0, c0) = *CAL.get_or_init(|| (Instant::now(), rdtsc()));
    let secs = t0.elapsed().as_secs_f64().max(1e-9);
    let hz = ((rdtsc().wrapping_sub(c0)) as f64 / secs).max(1.0);
    let (s, f) = measure_scope_overhead();
    (s as f64 / hz * 1e9, f as f64 / hz * 1e9)
}

/// Display name of bucket `i` (leading spaces mark nesting, as in [`dump`]).
pub fn name(i: usize) -> &'static str {
    NAMES.get(i).copied().unwrap_or("?")
}

/// The bucket `i` nests inside, if any — the report needs this to turn the
/// inclusive numbers into self-time, so our table can be read against libvpx's
/// (which is exclusive by construction). `None` = a disjoint top-level stage or
/// an INFO bucket that is not part of the partition.
pub fn parent(i: usize) -> Option<usize> {
    let s = |x: S| x as usize;
    match i {
        x if x == s(S::IntSearch) || x == s(S::SubpelSearch) => Some(s(S::MotionSearch)),
        x if x == s(S::Interp) => Some(s(S::SubpelSearch)),
        _ => None,
    }
}

/// Whether bucket `i` is one of the disjoint top-level stages that partition the
/// decision pass (the set [`dump`] uses as its denominator).
pub fn is_toplevel(i: usize) -> bool {
    TOPLEVEL.iter().any(|&s| s as usize == i)
}

/// Whether bucket `i` is an INFO bucket — an inclusive aggregate kept for
/// decomposing orchestration, deliberately NOT part of any partition. Summing
/// these with the top-level stages double-counts.
pub fn is_info(i: usize) -> bool {
    matches!(
        i,
        x if x == S::RdCost as usize
            || x == S::Sub8x8 as usize
            || x == S::DecideLeaf as usize
            || x == S::ModeMap as usize
            || x == S::SnapDrop as usize
            || x == S::PartCtx as usize
            || x == S::VarTree as usize
            || x == S::StoreMi as usize
            || x == S::MiCost as usize
    )
}

/// The two instrument-tax figures, measured against THIS machine at read time
/// rather than hardcoded. Shared by [`dump`] and [`snapshot`] so the printed
/// table and the analyzer's numbers can never drift apart.
///
/// * `.0` `ovh_self` — the bare `rdtsc`-pair latency that over-reports inside
///   each stage's OWN recorded span; subtract `calls × this` from the stage.
/// * `.1` `ovh_full` — a whole scope's cost (2 `rdtsc` + 2 atomic `fetch_add`)
///   that leaks into the PARENT/Total wall. With tens of millions of scope
///   entries this is a real, must-remove tax.
fn measure_scope_overhead() -> (u64, u64) {
    let dummy = AtomicU64::new(0);
    let (mut ovh_self, mut ovh_full) = (u64::MAX, u64::MAX);
    for _ in 0..4000 {
        let a = rdtsc();
        let b = rdtsc();
        ovh_self = ovh_self.min(b.wrapping_sub(a));
        let c = rdtsc();
        let s = rdtsc().max(1);
        dummy.fetch_add(rdtsc().wrapping_sub(s), Relaxed);
        dummy.fetch_add(1, Relaxed);
        let d = rdtsc();
        ovh_full = ovh_full.min(d.wrapping_sub(c));
    }
    (ovh_self, ovh_full)
}

// (Instant, rdtsc) calibration pair captured on first enabled scope → cycles↔ms.
static CAL: OnceLock<(Instant, u64)> = OnceLock::new();

#[cfg(target_arch = "x86_64")]
#[inline(always)]
fn rdtsc() -> u64 {
    unsafe { std::arch::x86_64::_rdtsc() }
}
#[cfg(not(target_arch = "x86_64"))]
#[inline(always)]
fn rdtsc() -> u64 {
    // Fallback: ns since a fixed epoch (profiler is a dev-only x86 tool anyway).
    std::time::Instant::now().elapsed().as_nanos() as u64
}

/// RAII timer: records elapsed cycles + one call into `stage` on drop. No-op unless enabled.
pub struct Scope {
    stage: usize,
    start: u64, // 0 ⇒ disabled
}

impl Scope {
    #[inline]
    pub fn new(stage: S) -> Scope {
        let on = enabled();
        if on {
            let _ = CAL.get_or_init(|| (Instant::now(), rdtsc()));
            Scope { stage: stage as usize, start: rdtsc().max(1) }
        } else {
            Scope { stage: stage as usize, start: 0 }
        }
    }
}

impl Drop for Scope {
    #[inline]
    fn drop(&mut self) {
        if self.start != 0 {
            let d = rdtsc().wrapping_sub(self.start);
            CYC[self.stage].fetch_add(d, Relaxed);
            COUNT[self.stage].fetch_add(1, Relaxed);
        }
    }
}

/// Time an expression under `stage` (no-op unless `VP9_PROF2`).
#[macro_export]
macro_rules! prof2 {
    ($stage:expr, $body:expr) => {{
        let _s = $crate::encode::prof::Scope::new($stage);
        $body
    }};
}

/// Print the accumulated per-stage table (called at end of encode when enabled).
pub fn dump() {
    if !enabled() {
        return;
    }
    {
        use std::sync::atomic::Ordering::Relaxed;
        let m = &crate::encode::frameenc::DEDUP;
        let (l, h, mt) = (m[0].load(Relaxed), m[1].load(Relaxed), m[2].load(Relaxed));
        if l > 0 {
            eprintln!("  dedup: emit_lookups={} key_hits={:.1}% value_matches={:.1}%",
                l, 100.0*h as f64/l as f64, 100.0*mt as f64/l as f64);
        }
    }
    let (t0, c0) = *CAL.get_or_init(|| (Instant::now(), rdtsc()));
    let secs = t0.elapsed().as_secs_f64().max(1e-9);
    let hz = (rdtsc().wrapping_sub(c0)) as f64 / secs; // measured TSC Hz

    // Two overhead measurements. `ovh_self` = the bare rdtsc-pair latency that
    // over-reports inside each stage's OWN recorded span (subtract from the stage).
    // `ovh_full` = a whole scope's cost (2 rdtsc + 2 atomic fetch_add) that leaks
    // into the PARENT/Total wall (subtract calls×ovh_full from Total). With tens of
    // millions of scope entries this is a real, must-remove instrument tax.
    let (ovh_self, ovh_full) = measure_scope_overhead();

    let _ = ovh_full; // (kept for the wall-overhead note below)
    let raw = |i: usize| CYC[i].load(Relaxed);
    let cnt = |i: usize| COUNT[i].load(Relaxed);
    let cor = |i: usize| raw(i).saturating_sub(cnt(i).saturating_mul(ovh_self));

    // Denominator = Σ of the (self-corrected) DISJOINT top-level stages. This is
    // stable run-to-run because each term is stable; deriving it by subtracting
    // `ovh_full × (tens of millions of calls)` from the raw wall was not (a few-cyc
    // calibration wobble × 45M calls swung the total ±30%). The raw Total wall and
    // its instrument tax are printed separately as context, not used as the base.
    let total = TOPLEVEL.iter().map(|&s| cor(s as usize)).sum::<u64>().max(1);
    let ms = |c: u64| c as f64 / hz * 1e3;
    let us_call = |c: u64, n: u64| if n > 0 { c as f64 / hz * 1e6 / n as f64 } else { 0.0 };

    eprintln!(
        "VP9_PROF2 (rdtsc @ {:.2} GHz; self-overhead {} cyc/scope removed; % = share of Σ measured primitives):",
        hz / 1e9,
        ovh_self,
    );
    for i in 1..N {
        let c = cor(i);
        eprintln!(
            "  {:16} {:8.1} ms  {:5.1}%  calls={:>11}  {:.4} us/call",
            NAMES[i],
            ms(c),
            c as f64 * 100.0 / total as f64,
            cnt(i),
            us_call(c, cnt(i)),
        );
    }
    // Context lines (NOT the denominator): the raw decision wall, the profiler's own
    // instrument tax, and the leftover unscoped orchestration.
    let wall = cor(S::Total as usize);
    let inner: u64 = (0..N).filter(|&i| i != S::Total as usize).map(cnt).sum();
    let instr = inner.saturating_mul(ovh_full);
    let orch = wall.saturating_sub(total).saturating_sub(instr);
    eprintln!(
        "  [context] Σ measured primitives = {:.0} ms (denominator)",
        ms(total)
    );
    eprintln!(
        "  [context] raw decision wall = {:.0} ms  |  profiler instrument tax ≈ {:.0} ms  |  unscoped orchestration ≈ {:.0} ms",
        ms(wall),
        ms(instr),
        ms(orch),
    );
}
