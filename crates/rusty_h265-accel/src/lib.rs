//! SIMD kernels for [`rusty_h265`](https://crates.io/crates/rusty_h265).
//!
//! The decoder core is `#![forbid(unsafe_code)]`; this crate is the seam where
//! acceleration is allowed to use `unsafe`, and it is confined to
//! `#[target_feature]` functions whose bounds the safe dispatcher proves.
//!
//! # Shape
//!
//! Every kernel comes in a set:
//!
//! - `*_scalar` — plain safe Rust. **The oracle and the fallback**, compiled on
//!   every target, and the only implementation when the `simd` feature is off.
//! - `*_sse2` / `*_avx2` (x86_64), `*_neon` (aarch64) — the twins. SSE2 and
//!   NEON are *baseline* on their architectures, so they need no runtime
//!   detection and no second code path production might never run; AVX2 is
//!   detected once and cached.
//! - a safe `pub fn` dispatcher that picks one and documents why the pointer
//!   arithmetic inside is in bounds.
//!
//! Every kernel is integer and exact: the SIMD twin performs the same
//! operations in the same order as the scalar one, so the gate is
//! `assert_eq!`, not a tolerance. `*_matches_scalar` tests pin that.
//!
//! # Where the dispatch sits
//!
//! A `#[target_feature]` function cannot be inlined into a caller that lacks
//! the feature, so the call is real. Every entry point here therefore takes a
//! whole **block** (a prediction unit, a transform block, a CTB row of SAO) —
//! the loop lives inside the feature boundary, never outside it.

#![cfg_attr(not(feature = "simd"), forbid(unsafe_code))]
#![deny(unsafe_op_in_unsafe_fn)]

pub mod deblock;
pub mod intra;
pub mod mc;
pub mod pixel;
pub mod sao;

/// The widest instruction set this build may use at runtime.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub enum Isa {
    /// Portable Rust. Every target, and the oracle everywhere.
    Scalar,
    /// x86-64 baseline (SSE2) or aarch64 baseline (NEON) — always available.
    Baseline,
    /// AVX2 + FMA-era x86-64.
    Avx2,
}

#[cfg(all(feature = "simd", target_arch = "x86_64"))]
mod detect {
    use super::Isa;
    use std::sync::atomic::{AtomicU8, Ordering};

    static CACHE: AtomicU8 = AtomicU8::new(u8::MAX);

    pub fn isa() -> Isa {
        // Detected once; afterwards a relaxed byte load. Callers hoist this
        // above their loops regardless (see the module docs).
        match CACHE.load(Ordering::Relaxed) {
            0 => Isa::Baseline,
            1 => Isa::Avx2,
            _ => {
                let v = if std::is_x86_feature_detected!("avx2") { 1 } else { 0 };
                CACHE.store(v, Ordering::Relaxed);
                if v == 1 {
                    Isa::Avx2
                } else {
                    Isa::Baseline
                }
            }
        }
    }
}

#[cfg(all(feature = "simd", target_arch = "aarch64"))]
mod detect {
    use super::Isa;
    /// NEON is mandatory on aarch64.
    pub fn isa() -> Isa {
        Isa::Baseline
    }
}

#[cfg(not(all(feature = "simd", any(target_arch = "x86_64", target_arch = "aarch64"))))]
mod detect {
    use super::Isa;
    pub fn isa() -> Isa {
        Isa::Scalar
    }
}

/// The instruction set the kernels will use on this machine. Cached; safe to
/// call often, but hoist it above a hot loop anyway.
#[inline]
pub fn isa() -> Isa {
    detect::isa()
}

/// One line for a benchmark's method output, and for the reachability census:
/// which kernels a run will actually execute.
pub fn describe() -> &'static str {
    match isa() {
        Isa::Avx2 => "rusty_h265-accel: AVX2",
        Isa::Baseline => {
            if cfg!(target_arch = "x86_64") {
                "rusty_h265-accel: SSE2"
            } else if cfg!(target_arch = "aarch64") {
                "rusty_h265-accel: NEON"
            } else {
                "rusty_h265-accel: baseline"
            }
        }
        Isa::Scalar => "rusty_h265-accel: scalar (simd feature off)",
    }
}

/// Deterministic per-kernel call counters — the byte census that
/// `codec-vectorize-kernel`'s REACHABILITY step demands. A kernel with a test
/// and a benchmark but a zero here is not deployed, whatever the call graph
/// looks like. Enabled with the `census` feature or `RH265_CENSUS=1`.
pub mod census {
    use std::sync::atomic::{AtomicU64, Ordering};

    macro_rules! counters {
        ($($name:ident),* $(,)?) => {
            $(pub static $name: AtomicU64 = AtomicU64::new(0);)*

            /// Every counter with its current value, for reporting.
            pub fn snapshot() -> Vec<(&'static str, u64)> {
                vec![$((stringify!($name), $name.load(Ordering::Relaxed))),*]
            }

            /// Zero every counter.
            pub fn reset() {
                $($name.store(0, Ordering::Relaxed);)*
            }
        };
    }

    counters!(
        MC_LUMA_SCALAR,
        MC_LUMA_SIMD,
        MC_CHROMA_SCALAR,
        MC_CHROMA_SIMD,
        MC_EDGE_PAD,
        MC_COPY_SCALAR,
        PUT_BI_FP_SCALAR,
        PUT_BI_FP_SIMD,
        MC_FULLPEL_UNI,
        MC_FULLPEL_BI,
        MC_COPY_SIMD,
        PUT_UNI_SCALAR,
        PUT_UNI_SIMD,
        PUT_BI_SCALAR,
        PUT_BI_SIMD,
        TX_SKIP_SCALAR,
        TX_SKIP_SIMD,
        PUT_WEIGHTED_SCALAR,
        PUT_WEIGHTED_SIMD,
        ADD_RESIDUAL_SCALAR,
        ADD_RESIDUAL_SIMD,
        ADD_RESIDUAL_DC_SCALAR,
        ADD_RESIDUAL_DC_SIMD,
        SAO_BAND_SCALAR,
        SAO_BAND_SIMD,
        SAO_EDGE_SCALAR,
        SAO_EDGE_SIMD,
        INTRA_ANGULAR_SCALAR,
        INTRA_ANGULAR_SIMD,
        INTRA_PLANAR_SCALAR,
        INTRA_PLANAR_SIMD,
        INTRA_DC_SCALAR,
        INTRA_DC_SIMD,
        SAMPLES_SAO,
        SAMPLES_MC,
        SAMPLES_ADD_RESIDUAL,
        SAMPLES_INTRA,
        // ---- content-adaptive ROUTES (the Great Gate inventory) -----------
        // Every one of these is a population count: which arm the content
        // actually selected. A route with a population and no fast arm is a
        // missing kernel; a route with a zero population is unreachable code.
        // Motion compensation, by fractional position (§8.5.3.3.3).
        RT_MC_FULLPEL,
        RT_MC_HORIZ,
        RT_MC_VERT,
        RT_MC_2D,
        // ...and by the shift the bit depth implies.
        RT_MC_SHIFT0,
        RT_MC_SHIFTN,
        // Residual, by transform kind (§8.6.4) and size.
        DEBLOCK_LUMA_SIMD,
        DEBLOCK_LUMA_SCALAR,
        RT_DEBLOCK_SKIP,
        RT_DEBLOCK_STRONG,
        RT_DEBLOCK_WEAK,
        CABAC_BYPASS_CALLS,
        CABAC_BYPASS_BINS,
        MC_FIR_H_ROWS,
        RT_MC_VSYM,
        RT_MC_VGEN,
        RT_MC_TAP_ELIDE,
        RT_MC_TAP_FULL,
        RT_MC_FY_NONE,
        RT_MC_FY_SYM,
        RT_MC_FY_ASYM,
        RT_TX_DC_ONLY,
        RT_TX_GENERAL,
        RT_TX_BYPASS,
        RT_TX_SKIP,
        RT_TX_DST,
        RT_TX_DCT,
        RT_TX_N4,
        RT_TX_N8,
        RT_TX_N16,
        RT_TX_N32,
        RT_TX_SCALED,
        RT_TX_FLAT,
        // Sparsity: coefficients the last-significant position says are live,
        // against the dense block they sit in.
        RT_TX_NZ_AREA,
        RT_TX_FULL_AREA,
        // Intra, by direction and by whether the references needed work.
        RT_INTRA_ANG_ROW,
        RT_INTRA_ANG_TRANSPOSED,
        RT_INTRA_ANG_COPY,
        RT_INTRA_REF_FILTERED,
        RT_INTRA_REF_PLAIN,
        RT_INTRA_ALL_AVAIL,
        RT_INTRA_SUBSTITUTED,
        RT_INTRA_DC_DEFERRED,
        RT_INTRA_DC_FILLED,
        // Loop filters.
        RT_SAO_OFF,
        RT_SAO_BAND,
        RT_SAO_EDGE,
        RT_SAO_INTERIOR,
        RT_SAO_RING,
        RT_SAO_PIC_BYPASS,
        RT_DEBLOCK_LUMA,
        RT_DEBLOCK_CHROMA,
        // Picture-level capability.
        RT_PIC_TILES,
        RT_PIC_WPP,
        RT_PIC_10BIT,
        RT_PIC_8BIT,
    );

    /// True when the counters should be updated. Read once and hoisted by the
    /// caller; the kernels themselves check a `const` in release builds where
    /// the feature is off, so an unused census costs nothing.
    #[inline]
    pub fn enabled() -> bool {
        #[cfg(feature = "census")]
        {
            true
        }
        #[cfg(not(feature = "census"))]
        {
            static ON: std::sync::OnceLock<bool> = std::sync::OnceLock::new();
            *ON.get_or_init(|| std::env::var_os("RH265_CENSUS").is_some())
        }
    }

    #[inline]
    pub fn bump(c: &AtomicU64, n: u64) {
        c.fetch_add(n, Ordering::Relaxed);
    }

    /// Records which arm of a two-way route the content selected.
    ///
    /// The Great Gate's decoder rule: a route is only a gate once its
    /// POPULATION is measured. `route(cond, a, b)` is the one-liner that makes
    /// every dispatch site say which way it went, so an arm with no population
    /// (unreachable) and a population with no fast arm (a missing kernel) both
    /// show up in the same table.
    #[inline]
    pub fn route(taken: bool, yes: &AtomicU64, no: &AtomicU64) {
        if enabled() {
            bump(if taken { yes } else { no }, 1);
        }
    }

    /// Records one arm of a route with more than two arms.
    #[inline]
    pub fn arm(c: &AtomicU64) {
        if enabled() {
            bump(c, 1);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn isa_is_stable_and_described() {
        let a = isa();
        for _ in 0..4 {
            assert_eq!(isa(), a);
        }
        assert!(describe().starts_with("rusty_h265-accel:"));
    }
}