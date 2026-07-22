//! Env-gated primitive profiler for the VP9 encoder hot path (`VP9_PROF2=1`).
//!
//! `rdtsc` cycle timers keyed by a fixed set of stages, accumulated with per-stage
//! call counts into process-global atomics, dumped by [`dump`]. `rdtsc` (not
//! `Instant`) because the innermost kernels are sub-µs — `Instant::as_micros()`
//! truncates a 0.05 µs call to 0, which silently hid transform/quantize time in
//! the "glue" bucket. Every probe is a no-op unless `VP9_PROF2` is set. At dump we
//! measure the empty-scope `rdtsc` overhead and subtract `calls × overhead` per
//! stage so the millions-of-calls kernels aren't inflated by the instrument.

use std::sync::atomic::{AtomicU64, Ordering::Relaxed};
use std::sync::OnceLock;
use std::time::Instant;

/// Hot-path stages we attribute time to. Keep in sync with [`NAMES`].
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
    Sub8x8,       // decide_sub8x8 (per-4×4 RD; may nest MC/SAD)
    SnapRestore,  // GLUE: entropy-context snapshot/restore around RD trials
    CtxUpdate,    // GLUE: entropy-context maintenance (set_ctx, partition ctx)
    MvRefs,       // GLUE: MV reference prediction (find_mv_refs, NEAREST/NEAR)
    Count,
}

const N: usize = S::Count as usize;
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
    "sub8x8",
    "snap_restore",
    "ctx_update",
    "mv_refs",
];

// The disjoint, non-nested top-level stages (so glue = TOTAL − Σ these). Excludes
// the nested motion children (int/subpel/interp) and TOTAL itself.
const TOPLEVEL: [S; 12] = [
    S::MotionSearch, S::Mc, S::FwdTx, S::Quantize, S::CoefCost, S::Trellis,
    S::InvTxRecon, S::IntraPred, S::Sub8x8, S::SnapRestore, S::CtxUpdate, S::MvRefs,
];

static CYC: [AtomicU64; N] = [const { AtomicU64::new(0) }; N];
static COUNT: [AtomicU64; N] = [const { AtomicU64::new(0) }; N];

thread_local! {
    static ENABLED: bool = std::env::var("VP9_PROF2").is_ok();
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
        let on = ENABLED.with(|&e| e);
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
    if !ENABLED.with(|&e| e) {
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
