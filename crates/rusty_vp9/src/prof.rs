//! Function-level stage profiler for the VP9 **decoder** hot path.
//!
//! Off unless `VP9_DPROF` is set or `set_enabled` is called, so a normal
//! decode pays one relaxed atomic load per scope and nothing else.
//!
//! **Exclusive self-time**, deliberately unlike the encoder's
//! [`encode::prof`](crate::encode::prof) (which is nested-inclusive with a
//! hand-maintained top-level partition). A stack holds the open stage; entering
//! a child closes the parent's running span and opens its own, so every cycle is
//! charged to exactly one bucket and the buckets sum to the measured wall. That
//! makes the percentages *share of decode* directly — the number a function-level
//! optimisation is prioritised by — and it matches the libvpx reference twin's
//! profiler (`_ref_libvpx/vpxprof.c`) bucket-for-bucket, so the two tables can be
//! read side by side without a nesting map in between.
//!
//! `rdtsc` rather than `Instant`: the innermost stages (one 4×4 inverse
//! transform, one coefficient group) are tens of nanoseconds, and a
//! microsecond-resolution clock truncates them to zero — which does not lose the
//! time, it silently *moves* it into the glue bucket.
//!
//! Single-threaded: the decoder core is one thread per stream, and the stack is
//! a thread-local, so a multi-threaded caller gets per-thread attribution rather
//! than corruption.

use std::cell::RefCell;
use std::sync::atomic::{AtomicU64, AtomicU8, Ordering::Relaxed};
use std::sync::OnceLock;
use std::time::Instant;

/// Decode stages. Names and ordering mirror the libvpx twin's decoder buckets so
/// the report can pair them by name.
#[derive(Clone, Copy, PartialEq, Eq)]
pub enum S {
    /// Unattributed decode glue — the residue. Always index 0.
    Other = 0,
    /// Uncompressed + compressed frame header (probability updates included).
    Header,
    /// Per-block mode / reference / motion-vector parse (`read_mode_info`).
    ModeInfo,
    /// Coefficient bool-decoding (`decode_coefs`) — the entropy core.
    Detokenize,
    /// Intra edge construction + directional/DC/TM prediction.
    IntraPred,
    /// Inter prediction: 8-tap sub-pel motion compensation.
    InterPred,
    /// Inverse transform + add-to-predictor (reconstruction).
    InvTxAdd,
    /// In-loop deblocking filter.
    LoopFilter,
    /// Backward probability adaptation at end of frame.
    Adapt,
    /// Reference-frame slot bookkeeping / plane copies between frames.
    RefUpdate,
    /// Per-frame allocation + zeroing of the reconstruction state (mode-info
    /// grid, the three frame-size planes, the above/left context arrays). Its
    /// own bucket because it is O(frame) work done once per frame OUTSIDE any
    /// codec kernel — the kind of cost that otherwise hides in the residue.
    FrameSetup,
    Count,
}

/// Number of stage buckets.
pub const N: usize = S::Count as usize;

const NAMES: [&str; N] = [
    "other/glue",
    "read_headers",
    "read_mode_info",
    "detokenize",
    "intra_pred",
    "inter_pred(mc)",
    "invtx_add",
    "loop_filter",
    "adapt_probs",
    "ref_update",
    "frame_setup(alloc)",
];

/// One slot past the reported stages: the "outside any decode" sink. Time spent
/// between `receive_frame` calls (demuxing, writing output, the caller's own
/// work) is charged here and never reported — otherwise the residue would grow
/// without bound from process start and swamp every real bucket.
const IDLE: usize = N;

static CYC: [AtomicU64; N + 1] = [const { AtomicU64::new(0) }; N + 1];
static COUNT: [AtomicU64; N + 1] = [const { AtomicU64::new(0) }; N + 1];

static ENABLED: AtomicU8 = AtomicU8::new(UNSET);
const UNSET: u8 = 2;

thread_local! {
    /// (stack of open stages, rdtsc at the last transition). The bottom of the
    /// stack is [`IDLE`]; the decoder pushes an [`S::Other`] scope around each
    /// frame, so unattributed work INSIDE a decode is the reported residue while
    /// work outside one is discarded. `last == 0` means "not started yet" — the
    /// first transition seeds it and charges nothing, because charging
    /// `now - 0` would attribute the machine's entire uptime to the residue.
    static STATE: RefCell<(Vec<usize>, u64)> = RefCell::new((vec![IDLE], 0));
}

// (Instant, rdtsc) pair captured on first use → cycles ↔ milliseconds.
static CAL: OnceLock<(Instant, u64)> = OnceLock::new();

#[cfg(target_arch = "x86_64")]
#[inline(always)]
fn rdtsc() -> u64 {
    // SAFETY: `_rdtsc` reads the timestamp counter; it has no memory operands,
    // no side effects, and is available on every x86_64 CPU.
    unsafe { std::arch::x86_64::_rdtsc() }
}
#[cfg(not(target_arch = "x86_64"))]
#[inline(always)]
fn rdtsc() -> u64 {
    // The profiler is a dev-only x86 tool; elsewhere fall back to a nanosecond
    // clock so the code still builds and reports something monotonic.
    static EPOCH: OnceLock<Instant> = OnceLock::new();
    EPOCH.get_or_init(Instant::now).elapsed().as_nanos() as u64
}

#[inline(always)]
fn enabled() -> bool {
    match ENABLED.load(Relaxed) {
        0 => false,
        1 => true,
        _ => {
            let on = std::env::var("VP9_DPROF").is_ok();
            ENABLED.store(on as u8, Relaxed);
            on
        }
    }
}

/// Turn the taps on or off for this process (the analyzer's `stages` pass).
pub fn set_enabled(on: bool) {
    ENABLED.store(on as u8, Relaxed);
    if on {
        let _ = CAL.get_or_init(|| (Instant::now(), rdtsc()));
        STATE.with(|st| {
            let mut st = st.borrow_mut();
            st.0.truncate(1);
            st.1 = rdtsc();
        });
    }
}

/// Zero every bucket and re-open the residue at "now".
pub fn reset() {
    for i in 0..=N {
        CYC[i].store(0, Relaxed);
        COUNT[i].store(0, Relaxed);
    }
    if enabled() {
        STATE.with(|st| {
            let mut st = st.borrow_mut();
            st.0.truncate(1);
            st.1 = rdtsc();
        });
    }
}

/// Per-stage `(milliseconds, calls)` since the last [`reset`].
///
/// The per-scope `rdtsc` latency is subtracted from each bucket: a scope's own
/// entry/exit reads land inside its recorded span, and with millions of 4×4
/// transform blocks that tax is a real fraction of the smallest buckets.
pub fn snapshot() -> [(f64, u64); N] {
    let (t0, c0) = *CAL.get_or_init(|| (Instant::now(), rdtsc()));
    let secs = t0.elapsed().as_secs_f64().max(1e-9);
    let hz = (rdtsc().wrapping_sub(c0)) as f64 / secs;
    let ovh = scope_overhead_cycles();
    let mut out = [(0.0f64, 0u64); N];
    for (i, o) in out.iter_mut().enumerate() {
        let calls = COUNT[i].load(Relaxed);
        let cyc = CYC[i].load(Relaxed).saturating_sub(calls.saturating_mul(ovh));
        *o = (cyc as f64 / hz * 1e3, calls);
    }
    out
}

/// Display name of bucket `i`.
pub fn name(i: usize) -> &'static str {
    NAMES.get(i).copied().unwrap_or("?")
}

/// Cost in cycles of one enter/exit pair, measured against THIS machine so the
/// tax reported next to the residue tracks the box rather than a guess.
fn scope_overhead_cycles() -> u64 {
    let mut best = u64::MAX;
    for _ in 0..4000 {
        let a = rdtsc();
        let b = rdtsc();
        best = best.min(b.wrapping_sub(a));
    }
    best
}

/// Charge the elapsed cycles to the stage on top of the stack and restart the
/// span. Shared by enter and exit — the only difference is what happens to the
/// stack afterwards.
#[inline]
fn transition(push: Option<usize>) {
    STATE.with(|st| {
        let mut st = st.borrow_mut();
        let now = rdtsc();
        let (stack, last) = &mut *st;
        if *last != 0 {
            let top = *stack.last().unwrap_or(&IDLE);
            CYC[top].fetch_add(now.wrapping_sub(*last), Relaxed);
        }
        *last = now;
        match push {
            Some(stage) => {
                stack.push(stage);
                COUNT[stage].fetch_add(1, Relaxed);
            }
            None => {
                if stack.len() > 1 {
                    stack.pop();
                }
            }
        }
    });
}

/// RAII stage timer. Opening one suspends the enclosing stage; dropping it
/// resumes the enclosing stage. No-op unless the profiler is enabled.
pub struct Scope {
    live: bool,
}

impl Scope {
    #[inline]
    pub fn new(stage: S) -> Scope {
        if !enabled() {
            return Scope { live: false };
        }
        let _ = CAL.get_or_init(|| (Instant::now(), rdtsc()));
        transition(Some(stage as usize));
        Scope { live: true }
    }
}

impl Drop for Scope {
    #[inline]
    fn drop(&mut self) {
        if self.live {
            transition(None);
        }
    }
}

/// Time an expression under `stage`. No-op unless the profiler is enabled.
macro_rules! dprof {
    ($stage:expr, $body:expr) => {{
        let _s = $crate::prof::Scope::new($stage);
        $body
    }};
}
pub(crate) use dprof;

/// Print the accumulated table to stderr (the `VP9_DPROF` end-of-decode dump).
pub fn dump() {
    if !enabled() {
        return;
    }
    let snap = snapshot();
    let total: f64 = snap.iter().map(|&(ms, _)| ms).sum::<f64>().max(1e-9);
    let calls: u64 = snap.iter().map(|&(_, c)| c).sum();
    eprintln!("VP9_DPROF — decoder stage profile (exclusive self-time):");
    eprintln!("  {:16} {:>10} {:>7} {:>14} {:>10}", "stage", "ms", "pct", "calls", "us/call");
    for (i, &(ms, c)) in snap.iter().enumerate() {
        if i != S::Other as usize && c == 0 {
            continue;
        }
        eprintln!(
            "  {:16} {:10.3} {:6.2}% {:14} {:10.4}",
            NAMES[i],
            ms,
            100.0 * ms / total,
            c,
            if c > 0 { ms * 1e3 / c as f64 } else { 0.0 }
        );
    }
    eprintln!("  {:16} {:10.3} {:6.2}% {:14}", "TOTAL", total, 100.0, calls);
}
