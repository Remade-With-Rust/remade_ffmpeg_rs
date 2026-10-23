//! **Stage profiler + kernel census** (feature `profile`; zero cost without it).
//!
//! Two instruments for the vectorisation campaign (`codec-vectorize-kernel`):
//!
//! * **Stages** — CPU time per pipeline stage, one scope per *call* around a
//!   whole loop (never per element, so the tap is not what gets measured).
//!   Encoder stages run on the frame-parallel worker threads and sum across
//!   them; shares are what matter.
//! * **Census** — elements processed by the SIMD arm vs the scalar arm of each
//!   kernel. A deterministic count: it proves a twin is *reached* from the
//!   shipping path, which no output gate can (both arms agree by design).

/// Pipeline stages.
#[derive(Clone, Copy)]
#[repr(usize)]
pub enum Stage {
    EncMdct,
    EncPsy,
    EncTns,
    EncXpow,
    EncEstimate,
    EncQuant,
    EncCodebook,
    DecIcs,
    DecSpectral,
    DecDequant,
    DecTools,
    DecImdct,
    DecSbrAnalysis,
    DecSbrHf,
    DecSbrSynthesis,
    DecPs,
    DecOutput,
}

/// Stage names, indexed by [`Stage`].
pub const STAGES: [&str; 17] = [
    "enc mdct (analyze_long/short)",
    "enc psy (perceptual offsets)",
    "enc tns analysis",
    "enc xpow (|x|^0.75)",
    "enc rate-loop estimate",
    "enc quantize (code_core)",
    "enc codebook select",
    "dec ics side info (section/sf/tns)",
    "dec spectral huffman",
    "dec dequant (|q|^4/3 x gain, pns)",
    "dec tools (ms/is/pred/ltp/tns)",
    "dec imdct + window",
    "dec sbr qmf analysis",
    "dec sbr hf gen + adjust",
    "dec sbr qmf synthesis",
    "dec ps",
    "dec output (interleave)",
];

/// Kernels with (or eligible for) a SIMD twin.
#[derive(Clone, Copy)]
#[repr(usize)]
pub enum Kernel {
    Quantize,
    Xpow,
    Fft,
    FmulWindow,
    QmfWindow,
}

/// Kernel names, indexed by [`Kernel`].
pub const KERNELS: [&str; 5] = [
    "quantize_band",
    "xpow",
    "radix2 fft",
    "fmul_window",
    "qmf_window",
];

/// Deterministic workload counters (what the stream asks the decoder to do).
#[derive(Clone, Copy)]
#[repr(usize)]
pub enum Counter {
    ChannelFrames,
    ShortFrames,
    Codewords,
    CodedCoeffs,
    PnsCoeffs,
    ZeroCoeffs,
    TnsChannels,
    SlowTuples,
    EscTuples,
}

/// Counter names, indexed by [`Counter`].
pub const COUNTERS: [&str; 9] = [
    "channel frames",
    "eight-short channel frames",
    "spectral codewords",
    "huffman-coded coeffs",
    "pns noise coeffs",
    "zero/intensity coeffs",
    "tns-active channel frames",
    "spectral tuples via generic path",
    "book-11 tuples with escapes",
];

#[cfg(feature = "profile")]
mod imp {
    use super::{Counter, Kernel, Stage, COUNTERS, KERNELS, STAGES};
    use std::sync::atomic::{AtomicU64, Ordering::Relaxed};
    use std::time::Instant;

    static NS: [AtomicU64; STAGES.len()] = [const { AtomicU64::new(0) }; STAGES.len()];
    static CALLS: [AtomicU64; STAGES.len()] = [const { AtomicU64::new(0) }; STAGES.len()];
    static SIMD: [AtomicU64; KERNELS.len()] = [const { AtomicU64::new(0) }; KERNELS.len()];
    static SCALAR: [AtomicU64; KERNELS.len()] = [const { AtomicU64::new(0) }; KERNELS.len()];
    static COUNT: [AtomicU64; COUNTERS.len()] = [const { AtomicU64::new(0) }; COUNTERS.len()];

    #[inline]
    pub fn bump(c: Counter, n: usize) {
        COUNT[c as usize].fetch_add(n as u64, Relaxed);
    }

    /// Read and clear the workload counters.
    #[must_use]
    pub fn take_counters() -> Vec<(&'static str, u64)> {
        (0..COUNTERS.len())
            .map(|i| (COUNTERS[i], COUNT[i].swap(0, Relaxed)))
            .collect()
    }

    /// Times its stage until dropped.
    pub struct Scope(usize, Instant);

    impl Drop for Scope {
        fn drop(&mut self) {
            NS[self.0].fetch_add(self.1.elapsed().as_nanos() as u64, Relaxed);
            CALLS[self.0].fetch_add(1, Relaxed);
        }
    }

    #[inline]
    #[must_use]
    pub fn scope(s: Stage) -> Scope {
        Scope(s as usize, Instant::now())
    }

    #[inline]
    pub fn count(k: Kernel, simd: bool, elems: usize) {
        let t = if simd { &SIMD } else { &SCALAR };
        t[k as usize].fetch_add(elems as u64, Relaxed);
    }

    /// One profile row: a name and two counters.
    pub type Row = (&'static str, u64, u64);

    /// Read and clear: per stage (name, ns, calls), per kernel (name, simd, scalar).
    #[must_use]
    pub fn take() -> (Vec<Row>, Vec<Row>) {
        let stages = (0..STAGES.len())
            .map(|i| (STAGES[i], NS[i].swap(0, Relaxed), CALLS[i].swap(0, Relaxed)))
            .collect();
        let kernels = (0..KERNELS.len())
            .map(|i| {
                (
                    KERNELS[i],
                    SIMD[i].swap(0, Relaxed),
                    SCALAR[i].swap(0, Relaxed),
                )
            })
            .collect();
        (stages, kernels)
    }
}

#[cfg(not(feature = "profile"))]
mod imp {
    use super::{Counter, Kernel, Stage};

    pub struct Scope;

    #[inline(always)]
    pub fn bump(_: Counter, _: usize) {}

    #[inline(always)]
    pub fn scope(_: Stage) -> Scope {
        Scope
    }

    #[inline(always)]
    pub fn count(_: Kernel, _: bool, _: usize) {}
}

pub use imp::*;
