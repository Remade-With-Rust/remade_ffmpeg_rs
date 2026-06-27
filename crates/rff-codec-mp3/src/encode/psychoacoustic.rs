//! Psychoacoustic model — the quality brain of the encoder.
//!
//! Estimates the masking threshold per scalefactor band (what quantization noise
//! the ear won't hear) and decides the window type (long vs short) from the
//! signal's transient/perceptual-entropy behaviour. The quantizer then shapes
//! noise to sit under these thresholds. This is the LAME "secret sauce" — a
//! faithful psymodel matters more for output quality than any other brick.

use crate::frame::{BlockType, SFB_LONG};

/// Per-granule perceptual analysis result.
#[derive(Debug, Clone, Default)]
pub struct PsyResult {
    /// Chosen window sequence for this granule.
    pub block_type: BlockType,
    /// Masking threshold (allowed noise energy) per long-block scalefactor band.
    pub thresholds: [f32; SFB_LONG],
    /// Perceptual entropy — the rough bit demand, used by reservoir budgeting.
    pub perceptual_entropy: f32,
}

/// Run the psychoacoustic model over one granule of PCM.
pub fn analyze(_pcm: &[f32]) -> PsyResult {
    // brick: FFT the PCM (long + short windows), spread energy across critical
    // bands, compute the masking threshold and SMR per band, derive perceptual
    // entropy, and decide long vs short (attack detection / pre-echo control).
    todo!("mp3 encode: psychoacoustic model")
}
