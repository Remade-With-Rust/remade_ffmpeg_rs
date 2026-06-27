//! Hybrid synthesis stage 2: the 32-band polyphase synthesis filterbank.
//!
//! Each pass takes 32 subband samples and produces 32 PCM samples. A matrixing
//! step (a 32→64 cosine transform) feeds the 1024-sample FIFO `V[]`; a windowing
//! step then dots a 512-tap slice of `V` (the `D[]` synthesis window) to emit the
//! 32 outputs. Eighteen passes per granule yield 576 PCM samples per channel.

use crate::frame::GRANULE_LINES;

/// Run the synthesis filterbank for one channel's granule, appending interleaved
/// PCM into `pcm`. `fifo` is the persistent `V[]` state for this channel.
pub fn polyphase(
    _time: &[f32; GRANULE_LINES],
    _fifo: &mut [f32; 1024],
    _pcm: &mut Vec<f32>,
    _ch: usize,
    _channels: usize,
) {
    // brick: for each of the 18 passes — matrix the 32 subband samples into V[]
    // (advance the FIFO by 64), gather the 512-tap window product via D[], write
    // 32 samples. Interleave by channel using `ch`/`channels`.
    todo!("mp3 decode: polyphase synthesis")
}
