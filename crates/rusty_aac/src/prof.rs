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
    DecDequant,
    DecTools,
    DecImdct,
    DecSbrAnalysis,
    DecSbrHf,
    DecSbrSynthesis,
    DecPs,
}

/// Stage names, indexed by [`Stage`].
pub const STAGES: [&str; 15] = [
    "enc mdct (analyze_long/short)",
    "enc psy (perceptual offsets)",
    "enc tns analysis",
    "enc xpow (|x|^0.75)",
    "enc rate-loop estimate",
    "enc quantize (code_core)",
    "enc codebook select",
    "dec ics (huffman + side info)",
    "dec dequant (|q|^4/3 x gain, pns)",
    "dec tools (ms/is/pred/ltp/tns)",
    "dec imdct + window",
    "dec sbr qmf analysis",
    "dec sbr hf gen + adjust",
    "dec sbr qmf synthesis",
    "dec ps",
];

/// Kernels with (or eligible for) a SIMD twin.
#[derive(Clone, Copy)]
#[repr(usize)]
pub enum Kernel {
    Quantize,
    Xpow,
    Fft,
    FmulWindow,
}

/// Kernel names, indexed by [`Kernel`].
pub const KERNELS: [&str; 4] = ["quantize_band", "xpow", "radix2 fft", "fmul_window"];

#[cfg(feature = "profile")]
mod imp {
    use super::{Kernel, Stage, KERNELS, STAGES};
    use std::sync::atomic::{AtomicU64, Ordering::Relaxed};
    use std::time::Instant;

    static NS: [AtomicU64; STAGES.len()] = [const { AtomicU64::new(0) }; STAGES.len()];
    static CALLS: [AtomicU64; STAGES.len()] = [const { AtomicU64::new(0) }; STAGES.len()];
    static SIMD: [AtomicU64; KERNELS.len()] = [const { AtomicU64::new(0) }; KERNELS.len()];
    static SCALAR: [AtomicU64; KERNELS.len()] = [const { AtomicU64::new(0) }; KERNELS.len()];

    /// Times its stage until dropped.
    pub struct Scope(usize, Instant);

    impl Drop for Scope {
        fn drop(&mut self) {
            NS[self.0].fetch_add(self.1.elapsed().as_nanos() as u64, Relaxed);
            CALLS[self.0].fetch_add(1, Relaxed);
        }
    }

    #[inline]
    pub fn scope(s: Stage) -> Scope {
        Scope(s as usize, Instant::now())
    }

    #[inline]
    pub fn count(k: Kernel, simd: bool, elems: usize) {
        let t = if simd { &SIMD } else { &SCALAR };
        t[k as usize].fetch_add(elems as u64, Relaxed);
    }

    /// Read and clear: per stage (name, ns, calls), per kernel (name, simd, scalar).
    pub fn take() -> (Vec<(&'static str, u64, u64)>, Vec<(&'static str, u64, u64)>) {
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
    use super::{Kernel, Stage};

    pub struct Scope;

    #[inline(always)]
    pub fn scope(_: Stage) -> Scope {
        Scope
    }

    #[inline(always)]
    pub fn count(_: Kernel, _: bool, _: usize) {}
}

pub use imp::*;
