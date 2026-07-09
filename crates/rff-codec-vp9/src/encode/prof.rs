//! Env-gated primitive profiler for the VP9 encoder hot path (`VP9_PROF2=1`).
//!
//! Coarse `Instant` timers keyed by a fixed set of stages, accumulated (µs) with
//! per-stage call counts into process-global atomics, dumped by [`dump`]. Every
//! probe is a zero-cost no-op unless `VP9_PROF2` is set, so it can live in the
//! tree. Timers are placed on *mid-level* functions (per-block / per-candidate,
//! thousands of calls) so the `Instant::now()` overhead stays in the noise — do
//! NOT wrap the innermost per-coefficient kernels with it.

use std::sync::atomic::{AtomicU64, Ordering::Relaxed};
use std::time::Instant;

/// Hot-path stages we attribute time to. Keep in sync with [`NAMES`].
#[derive(Clone, Copy)]
pub enum S {
    MotionSearch = 0, // search_mv (SAD full-search) — aggregate
    IntSearch,        // integer full-pel grid (block_sad loop)
    SubpelSearch,     // 1/4-pel refinement (predicted_sad loop)
    Interp,           // predict_block (8-tap subpel interp) inside the refinement
    Mc,               // inter_predict_mv (8-tap subpel motion compensation)
    FwdTx,            // forward_transform kernel
    Quantize,         // quantize kernel
    CoefCost,         // coef_cost (RD bit costing, trial path)
    Trellis,          // trellis_eob (per-coef RD + inverse transforms)
    InvTxRecon,       // inverse transform + add (reconstruction)
    IntraPred,        // build_intra_edges + intra predict
    Sub8x8,           // decide_sub8x8 (per-4×4 RD search; may nest MC/SAD)
    Count,
}

const N: usize = S::Count as usize;
const NAMES: [&str; N] = [
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
];

static TIME_US: [AtomicU64; N] = [const { AtomicU64::new(0) }; N];
static COUNT: [AtomicU64; N] = [const { AtomicU64::new(0) }; N];

thread_local! {
    static ENABLED: bool = std::env::var("VP9_PROF2").is_ok();
}

/// RAII timer: records elapsed µs + one call into `stage` on drop. No-op unless enabled.
pub struct Scope {
    stage: usize,
    start: Option<Instant>,
}

impl Scope {
    #[inline]
    pub fn new(stage: S) -> Scope {
        let on = ENABLED.with(|&e| e);
        Scope {
            stage: stage as usize,
            start: if on { Some(Instant::now()) } else { None },
        }
    }
}

impl Drop for Scope {
    #[inline]
    fn drop(&mut self) {
        if let Some(t) = self.start {
            TIME_US[self.stage].fetch_add(t.elapsed().as_micros() as u64, Relaxed);
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
    let total: u64 = (0..N).map(|i| TIME_US[i].load(Relaxed)).sum::<u64>().max(1);
    eprintln!("VP9_PROF2 primitive breakdown (cum, overlapping timers nest):");
    let mut rows: Vec<(usize, u64, u64)> = (0..N)
        .map(|i| (i, TIME_US[i].load(Relaxed), COUNT[i].load(Relaxed)))
        .collect();
    rows.sort_by(|a, b| b.1.cmp(&a.1));
    for (i, us, cnt) in rows {
        eprintln!(
            "  {:14} {:8.0} ms  {:5.1}%  calls={:>10}  {:.3} us/call",
            NAMES[i],
            us as f64 / 1e3,
            us as f64 * 100.0 / total as f64,
            cnt,
            if cnt > 0 { us as f64 / cnt as f64 } else { 0.0 },
        );
    }
}
