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
use crate::header::FrameHeader;

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

/// Quantize one granule at `global_gain` into integer levels (sign-separated).
/// Forward of the decoder's `xr = sign·|is|^(4/3)·2^(0.25·(gain−210))`.
fn quantize_at(freq: &[f32; GRANULE_LINES], gain: i32) -> [i32; GRANULE_LINES] {
    let scale_inv = 2f64.powf(-0.25 * (gain - 210) as f64);
    let mut coeffs = [0i32; GRANULE_LINES];
    for (i, &x) in freq.iter().enumerate() {
        let mag = quantize_level(x.abs() as f64 * scale_inv);
        coeffs[i] = if x < 0.0 { -mag } else { mag };
    }
    coeffs
}

/// Build a granule (coeffs + Huffman layout) at a candidate `global_gain`.
fn build(header: &FrameHeader, freq: &[f32; GRANULE_LINES], gain: i32) -> QuantizedGranule {
    let coeffs = quantize_at(freq, gain);
    let mut side = super::huffman::select(header, &coeffs);
    side.global_gain = gain as u8;
    side.scalefac_compress = 0; // flat scalefactors (C1/C2 do no shaping)
    QuantizedGranule {
        coeffs,
        side,
        scalefactors: [0; 39],
    }
}

/// Huffman bit cost of a candidate granule.
fn cost(header: &FrameHeader, q: &QuantizedGranule) -> usize {
    let mut w = crate::bitio::BitWriter::new();
    super::huffman::encode(q, header, &mut w)
}

/// Largest non-clipping quantized level. Above this the value would saturate at
/// `MAX_LEVEL`, losing precision — so a gain that produces it is *too fine*.
const MAX_UNCLIPPED: i32 = 8191;

/// **C2 — the rate loop.** Find the *smallest* `global_gain` (finest step, best
/// quality) whose Huffman-coded spectrum fits `bit_budget`, then fill the granule.
///
/// Bits decrease monotonically as the gain rises (coarser step → smaller levels →
/// fewer bits), so a binary search pins the boundary. There is no outer distortion
/// loop yet (that's Q6) and scalefactors stay flat — this is rate control only.
pub fn loops(
    header: &FrameHeader,
    freq: &[f32; GRANULE_LINES],
    _psy: &PsyResult,
    bit_budget: usize,
) -> QuantizedGranule {
    // A gain is acceptable when nothing clips *and* the spectrum fits the budget.
    // Both improve as the gain rises (coarser step → smaller levels → fewer bits,
    // no clipping), so the smallest acceptable gain is the best-quality one.
    // Crucially this rejects tiny gains where everything clamps to MAX_LEVEL but
    // still costs few bits (which a budget-only test would wrongly accept).
    let ok = |g: i32| {
        let q = build(header, freq, g);
        q.coeffs.iter().all(|&c| c.abs() <= MAX_UNCLIPPED) && cost(header, &q) <= bit_budget
    };

    let (mut lo, mut hi) = (0i32, 255i32);
    while lo < hi {
        let mid = (lo + hi) / 2;
        if ok(mid) {
            hi = mid;
        } else {
            lo = mid + 1;
        }
    }

    let mut q = build(header, freq, lo);
    // Scalefactors are zero-length here, so part2_3_length == the Huffman bits.
    q.side.part2_3_length = cost(header, &q) as u16;
    q
}

#[cfg(test)]
mod c2_tests {
    use super::*;
    use crate::decode::scalefactors::ScaleFactors;
    use crate::frame::ChannelMode;
    use crate::header::MpegVersion;

    fn hdr() -> FrameHeader {
        FrameHeader {
            version: MpegVersion::V1,
            crc_protected: false,
            bitrate_kbps: 128,
            sample_rate: 44100,
            padding: false,
            channel_mode: ChannelMode::Mono,
            copyright: false,
            original: true,
            emphasis: 0,
        }
    }

    #[test]
    fn spectrum_roundtrip_through_quantizer() {
        // A small synthetic spectrum: quantize then requantize should recover it.
        let header = hdr();
        let mut freq = [0f32; GRANULE_LINES];
        freq[40] = 5.0;
        freq[41] = -3.0;
        freq[100] = 1.2;
        freq[200] = 0.6;

        let psy = PsyResult::default();
        let q = loops(&header, &freq, &psy, 100_000); // generous budget → fine gain
        eprintln!(
            "[C2dbg] gain={} part2_3={} nz_coeffs={}",
            q.side.global_gain,
            q.side.part2_3_length,
            q.coeffs.iter().filter(|&&c| c != 0).count()
        );

        // Requantize the way the decoder does.
        let mut out = [0f32; GRANULE_LINES];
        let nz = q.coeffs.iter().rposition(|&c| c != 0).map_or(0, |i| i + 1);
        crate::decode::requantize::apply(
            &header,
            &q.side,
            &ScaleFactors::default(),
            &q.coeffs,
            nz,
            &mut out,
        );

        let mut maxerr = 0f32;
        for i in 0..GRANULE_LINES {
            maxerr = maxerr.max((out[i] - freq[i]).abs());
        }
        eprintln!(
            "[C2dbg] requant maxerr={maxerr} out[40]={} out[41]={}",
            out[40], out[41]
        );
        assert!(maxerr < 0.2, "spectrum round-trip error {maxerr}");
    }
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
