//! Decode stage profiler — the `profile` feature only (measurement, never
//! shipped). Off, every scope is a zero-sized no-op and every counter call is
//! empty, so the release decoder is unchanged.
//!
//! On, each [`scope`] adds its wall-clock span to a per-thread bucket and
//! bumps the bucket's call count; [`count`] adds to a per-thread workload
//! counter. Scopes sit at frame / subframe granularity (a few per channel per
//! frame), so the timer tax is ~2 `Instant::now()` per scope — print the call
//! counts beside the times and subtract `calls × scope cost` before reading a
//! residue (`codec-measurement` §6).

/// Decode pipeline stages.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
#[allow(dead_code)]
pub enum Stage {
    /// Whole `next_frame` call.
    Frame,
    /// Frame header parse + CRC-8.
    Header,
    /// Subframe header, warm-up samples, LPC coefficients.
    SubHeader,
    /// Partitioned Rice residual decode.
    Residual,
    /// Fixed-predictor restore.
    Fixed,
    /// LPC restore.
    Lpc,
    /// Wasted-bits shift, constant / verbatim fill.
    Misc,
    /// Frame CRC-16.
    Crc16,
    /// Stereo decorrelation.
    Decorr,
    /// Copy of the decoded block into the caller's output.
    Output,
}

/// Number of [`Stage`]s.
#[allow(dead_code)]
pub const STAGES: usize = 10;

/// Stage names, in [`Stage`] order.
#[allow(dead_code)]
pub const STAGE_NAMES: [&str; STAGES] = [
    "frame(total)",
    "header+crc8",
    "subframe hdr",
    "residual",
    "fixed restore",
    "lpc restore",
    "misc",
    "crc16",
    "decorrelate",
    "output copy",
];

/// Workload counters.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
#[allow(dead_code)]
pub enum Ctr {
    Frames,
    Subframes,
    Residuals,
    Partitions,
    EscapePartitions,
    Constant,
    Verbatim,
    FixedSub,
    LpcSub,
    /// Sum of LPC orders over LPC subframes.
    LpcOrderSum,
    /// LPC subframes whose order exceeds 12 (the generic loop).
    LpcHighOrder,
    /// Samples restored by an LPC whose accumulator provably fits 32 bits.
    LpcSamples32,
    /// Samples restored by an LPC needing the 64-bit accumulator.
    LpcSamples64,
    /// Frames with each stereo assignment: independent / left-side / side-right / mid-side.
    Independent,
    LeftSide,
    SideRight,
    MidSide,
    /// Bytes through CRC-16.
    CrcBytes,
    /// Rice codes that missed the single-window fast path.
    RiceSlow,
}

/// Number of [`Ctr`]s.
#[allow(dead_code)]
pub const CTRS: usize = 19;

/// Counter names, in [`Ctr`] order.
#[allow(dead_code)]
pub const CTR_NAMES: [&str; CTRS] = [
    "frames",
    "subframes",
    "residuals",
    "partitions",
    "escape partitions",
    "constant subframes",
    "verbatim subframes",
    "fixed subframes",
    "lpc subframes",
    "lpc order sum",
    "lpc order > 12",
    "lpc samples (32-bit acc)",
    "lpc samples (64-bit acc)",
    "frames independent",
    "frames left/side",
    "frames side/right",
    "frames mid/side",
    "crc16 bytes",
    "rice slow path",
];

#[cfg(feature = "profile")]
mod imp {
    use super::{Ctr, Stage, CTRS, STAGES};
    use std::cell::Cell;
    use std::time::Instant;

    std::thread_local! {
        static NS: [Cell<u64>; STAGES] = const { [const { Cell::new(0) }; STAGES] };
        static CALLS: [Cell<u64>; STAGES] = const { [const { Cell::new(0) }; STAGES] };
        static CTR: [Cell<u64>; CTRS] = const { [const { Cell::new(0) }; CTRS] };
    }

    pub struct Guard(Stage, Instant);

    impl Drop for Guard {
        #[inline]
        fn drop(&mut self) {
            let ns = self.1.elapsed().as_nanos() as u64;
            let i = self.0 as usize;
            NS.with(|b| b[i].set(b[i].get() + ns));
            CALLS.with(|b| b[i].set(b[i].get() + 1));
        }
    }

    #[inline]
    pub fn scope(s: Stage) -> Guard {
        Guard(s, Instant::now())
    }

    #[inline]
    pub fn count(c: Ctr, n: u64) {
        let i = c as usize;
        CTR.with(|b| b[i].set(b[i].get() + n));
    }

    /// Zero this thread's buckets and counters.
    pub fn reset() {
        NS.with(|b| b.iter().for_each(|c| c.set(0)));
        CALLS.with(|b| b.iter().for_each(|c| c.set(0)));
        CTR.with(|b| b.iter().for_each(|c| c.set(0)));
    }

    /// This thread's `(ns, calls)` per stage and the counters.
    pub fn snapshot() -> ([(u64, u64); STAGES], [u64; CTRS]) {
        let mut st = [(0, 0); STAGES];
        NS.with(|b| st.iter_mut().zip(b).for_each(|(s, c)| s.0 = c.get()));
        CALLS.with(|b| st.iter_mut().zip(b).for_each(|(s, c)| s.1 = c.get()));
        let mut ct = [0; CTRS];
        CTR.with(|b| ct.iter_mut().zip(b).for_each(|(s, c)| *s = c.get()));
        (st, ct)
    }
}

#[cfg(not(feature = "profile"))]
mod imp {
    use super::{Ctr, Stage};

    pub struct Guard;

    // Empty, so the call sites can end a scope with `drop(guard)` in both
    // builds; it inlines to nothing.
    impl Drop for Guard {
        #[inline(always)]
        fn drop(&mut self) {}
    }

    #[inline(always)]
    pub fn scope(_: Stage) -> Guard {
        Guard
    }

    #[inline(always)]
    pub fn count(_: Ctr, _: u64) {}
}

pub(crate) use imp::{count, scope};
#[cfg(feature = "profile")]
pub use imp::{reset, snapshot};
