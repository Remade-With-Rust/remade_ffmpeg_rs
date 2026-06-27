//! Analysis polyphase filterbank — the mirror of decode's synthesis stage.
//!
//! Splits 32 PCM samples into 32 subband samples per pass (18 passes per
//! granule). Each pass windows a 512-tap slice of the input FIFO with the
//! analysis window, partial-sums to 64 values, then a 32×64 cosine matrix
//! produces the 32 subband outputs.

use crate::frame::{SUBBANDS, SUBBAND_LINES};

/// Analyze one granule of PCM into subband samples `[subband][line]`,
/// advancing the channel's filterbank FIFO.
pub fn analyze(_pcm: &[f32], _fifo: &mut [f32; 512]) -> [[f32; SUBBAND_LINES]; SUBBANDS] {
    // brick: 18 passes — shift 32 new samples into the FIFO, window with the
    // analysis taps, fold to 64, apply the cosine matrix to get 32 subbands.
    todo!("mp3 encode: analysis filterbank")
}
