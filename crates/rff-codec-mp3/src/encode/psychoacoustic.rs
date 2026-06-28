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
///
/// **C1 — the trivial model.** Always a long block, a flat (zero) masking
/// threshold, and perceptual entropy = signal energy. It satisfies the
/// [`PsyResult`] contract so the rest of the pipeline runs end to end; the real
/// FFT-based masking model (Q1–Q5) replaces it on Floor 4. There is deliberately
/// no perceptual shaping here — the encoder is correct-but-dumb at this stage.
pub fn analyze(pcm: &[f32]) -> PsyResult {
    let energy: f32 = pcm
        .iter()
        .take(crate::frame::GRANULE_LINES)
        .map(|x| x * x)
        .sum();
    PsyResult {
        block_type: BlockType::Long,
        thresholds: [0.0; SFB_LONG],
        perceptual_entropy: energy,
    }
}
