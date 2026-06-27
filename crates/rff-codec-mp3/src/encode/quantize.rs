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

use crate::frame::{GranuleSideInfo, GRANULE_LINES};

use super::psychoacoustic::PsyResult;

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
