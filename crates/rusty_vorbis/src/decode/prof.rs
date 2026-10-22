//! Stage profiler for the decoder (feature `profile`; zero cost when off).
//!
//! Each scope is one `rdtsc` pair (x86-64) accumulated into a per-stage
//! counter. Scopes sit at per-packet / per-channel granularity (~10 per
//! channel per packet), so the profiler's own tax is a few percent; the
//! per-codeword work inside residue decode is NOT individually scoped.

#[cfg(feature = "profile")]
mod imp {
    use std::sync::atomic::{AtomicU64, Ordering};

    pub const NAMES: [&str; 9] = [
        "header+floor decode",
        "residue decode",
        "residue de-interleave",
        "inverse coupling",
        "floor synthesis (x spectrum)",
        "imdct (dct-iv)",
        "unfold + window/OLA",
        "interleave",
        "other (spectra zero, setup)",
    ];
    static CYC: [AtomicU64; 9] = [const { AtomicU64::new(0) }; 9];
    static CALLS: [AtomicU64; 9] = [const { AtomicU64::new(0) }; 9];

    #[inline(always)]
    pub fn now() -> u64 {
        #[cfg(target_arch = "x86_64")]
        // SAFETY: rdtsc has no preconditions.
        unsafe {
            std::arch::x86_64::_rdtsc()
        }
        #[cfg(not(target_arch = "x86_64"))]
        {
            use std::time::Instant;
            static T0: std::sync::OnceLock<Instant> = std::sync::OnceLock::new();
            T0.get_or_init(Instant::now).elapsed().as_nanos() as u64
        }
    }

    #[inline(always)]
    pub fn add(stage: usize, t0: u64) -> u64 {
        let t = now();
        CYC[stage].fetch_add(t.wrapping_sub(t0), Ordering::Relaxed);
        CALLS[stage].fetch_add(1, Ordering::Relaxed);
        t
    }

    /// `(name, cycles, calls)` per stage.
    pub fn snapshot() -> Vec<(&'static str, u64, u64)> {
        (0..NAMES.len())
            .map(|i| {
                (
                    NAMES[i],
                    CYC[i].load(Ordering::Relaxed),
                    CALLS[i].load(Ordering::Relaxed),
                )
            })
            .collect()
    }

    pub fn reset() {
        for i in 0..NAMES.len() {
            CYC[i].store(0, Ordering::Relaxed);
            CALLS[i].store(0, Ordering::Relaxed);
        }
    }
}

#[cfg(not(feature = "profile"))]
mod imp {
    #[inline(always)]
    pub fn now() -> u64 {
        0
    }
    #[inline(always)]
    pub fn add(_stage: usize, t0: u64) -> u64 {
        t0
    }
}

pub(crate) use imp::{add, now};

#[cfg(feature = "profile")]
pub use imp::{reset, snapshot};

pub(crate) const FLOOR_DECODE: usize = 0;
pub(crate) const RESIDUE: usize = 1;
pub(crate) const DEINTERLEAVE: usize = 2;
pub(crate) const COUPLING: usize = 3;
pub(crate) const FLOOR_SYNTH: usize = 4;
pub(crate) const IMDCT: usize = 5;
pub(crate) const OLA: usize = 6;
pub(crate) const INTERLEAVE: usize = 7;
pub(crate) const OTHER: usize = 8;
