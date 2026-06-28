//! The two-loop quantizer — rate control + noise shaping.
//!
//! * **Inner loop (rate):** raise the quantization step until the Huffman-coded
//!   spectrum fits the granule's bit budget.
//! * **Outer loop (distortion):** raise per-band scalefactors where quantization
//!   noise exceeds the psychoacoustic threshold, re-running the inner loop, until
//!   noise is masked everywhere or no scalefactor budget remains.
//!
//! Produces the quantized integer spectrum plus the side-info fields (global
//! gain, scalefactors, scalefac_compress, block flags) that describe it.

use std::sync::OnceLock;

use crate::frame::{GranuleSideInfo, GRANULE_LINES};

use super::psychoacoustic::PsyResult;

// ── Brick N4: the nonuniform quantizer power law ──────────────────────────────
//
// The decoder requantizes `xr = sign(is)·|is|^(4/3)·scale` (see
// `decode/requantize.rs`). The encoder inverts the `|is|^(4/3)` core: a magnitude
// `xr` quantizes to the integer level `ix = nint(|xr|^(3/4) − BIAS)`. The two are
// exact inverses on the integer lattice — `quantize_level(requant_magnitude(ix))
// == ix` for every representable `ix` — which is N4's verification gate. The
// global-gain / scalefactor `scale` is applied by the rate loop (C2); N4 is the
// unit-step power law it builds on.

/// Largest Huffman magnitude reachable with `linbits` (matches the decoder).
pub const MAX_LEVEL: i32 = 8206;

/// ISO rounding bias subtracted before the round in the forward quantizer
/// (ISO/IEC 11172-3 2.4.2.7). It biases the decision boundary so the truncating
/// `nint` recovers the intended level.
pub const QUANT_BIAS: f64 = 0.0946;

/// `|level|^(4/3)`, table-backed for the `0..=MAX_LEVEL` magnitudes — the
/// requantization power law (the same curve the decoder applies, factored out so
/// the encoder's rate loop can predict what the decoder will reconstruct).
pub fn requant_magnitude(level: i32) -> f64 {
    static T: OnceLock<Vec<f64>> = OnceLock::new();
    let t = T.get_or_init(|| {
        (0..=MAX_LEVEL as usize)
            .map(|i| (i as f64).powf(4.0 / 3.0))
            .collect()
    });
    let a = level.unsigned_abs() as usize;
    t.get(a)
        .copied()
        .unwrap_or_else(|| (a as f64).powf(4.0 / 3.0))
}

/// Forward-quantize a (positive) frequency-line magnitude to its integer level
/// under unit step: `ix = nint(|xr|^(3/4) − BIAS)`, clamped to `[0, MAX_LEVEL]`.
/// The sign is carried separately, exactly as the bitstream does.
pub fn quantize_level(xr: f64) -> i32 {
    let m = xr.abs().powf(0.75) - QUANT_BIAS;
    if m <= 0.0 {
        0
    } else {
        (m.round() as i32).clamp(0, MAX_LEVEL)
    }
}

/// One granule's quantized output.
#[derive(Debug, Clone)]
pub struct QuantizedGranule {
    /// Quantized integer spectrum (`is`), 576 lines.
    pub coeffs: [i32; GRANULE_LINES],
    /// Side-info describing how to dequantize it (gain, tables, regions, flags).
    pub side: GranuleSideInfo,
    /// Scalefactors per band (long: 22; short: 3×13 packed).
    pub scalefactors: [u8; 39],
}

impl Default for QuantizedGranule {
    fn default() -> Self {
        // Arrays larger than 32 don't derive Default.
        QuantizedGranule {
            coeffs: [0; GRANULE_LINES],
            side: GranuleSideInfo::default(),
            scalefactors: [0; 39],
        }
    }
}

/// Quantize one granule's frequency lines under `psy`'s thresholds into at most
/// `bit_budget` bits.
pub fn loops(
    _freq: &[f32; GRANULE_LINES],
    _psy: &PsyResult,
    _bit_budget: usize,
) -> QuantizedGranule {
    // brick: nonuniform-quantize via x^(3/4); inner loop adjusts global_gain to
    // hit the bit budget (Huffman-cost estimate per candidate); outer loop pushes
    // scalefactors where band noise > threshold; pick region boundaries and
    // Huffman tables; fill GranuleSideInfo. Default to QuantizedGranule::default.
    todo!("mp3 encode: rate/distortion quantization loops")
}

#[cfg(test)]
mod n4_tests {
    use super::*;

    #[test]
    fn requant_magnitude_matches_power_law() {
        for level in [0, 1, 2, 3, 17, 255, 1024, MAX_LEVEL] {
            let expect = (level as f64).powf(4.0 / 3.0);
            assert!((requant_magnitude(level) - expect).abs() < 1e-9);
        }
        // Sign is carried separately, so the magnitude ignores it.
        assert_eq!(requant_magnitude(-3), requant_magnitude(3));
    }

    #[test]
    fn forward_inverse_round_trip_on_the_lattice() {
        // The verification gate: every representable level survives
        // requantize → quantize unchanged. If the BIAS or the power law were
        // wrong, some level would round to a neighbour.
        for level in 0..=MAX_LEVEL {
            let xr = requant_magnitude(level);
            assert_eq!(
                quantize_level(xr),
                level,
                "round-trip failed at level {level} (xr={xr})"
            );
        }
    }

    #[test]
    fn quantizer_clamps_and_zeroes() {
        assert_eq!(quantize_level(0.0), 0);
        // A value just below 1^(4/3) still rounds to 0 (below the first lattice
        // point, the bias pulls it under 0.5).
        assert_eq!(quantize_level(0.3), 0);
        // Saturates at MAX_LEVEL rather than overflowing.
        assert_eq!(quantize_level(1.0e9), MAX_LEVEL);
    }
}
