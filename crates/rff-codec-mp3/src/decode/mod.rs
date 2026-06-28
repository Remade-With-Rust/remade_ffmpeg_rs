//! MP3 decode pipeline.
//!
//! One frame flows through these bricks in order:
//!
//! ```text
//!  header ─▶ side info ─▶ bit reservoir ─▶ Huffman ─▶ scalefactors ─▶
//!  requantize ─▶ stereo (MS/intensity) ─▶ alias reduction ─▶
//!  hybrid IMDCT (+overlap) ─▶ polyphase synthesis ─▶ PCM
//! ```
//!
//! State that persists across frames lives on [`Mp3Decode`]: the bit reservoir,
//! the per-channel IMDCT overlap, and the synthesis filterbank FIFO.

use rff_core::{Error, Result};

use crate::frame::{GranuleSpectrum, SideInfo, GRANULE_LINES};
use crate::header::FrameHeader;

pub mod antialias;
pub mod codebooks;
pub mod huffman;
pub mod imdct;
pub mod requantize;
pub mod reservoir;
pub mod scalefactors;
pub mod sideinfo;
pub mod stereo;
pub mod synth_window;
pub mod synthesis;

/// Persistent decoder state across frames.
pub struct Mp3Decode {
    /// Carries leftover main-data bytes between frames (`main_data_begin`).
    reservoir: reservoir::Reservoir,
    /// Previous granule's IMDCT tail for overlap-add, `[channel][line]`.
    imdct_overlap: [[f32; GRANULE_LINES]; 2],
    /// Synthesis filterbank FIFO `V[]`, `[channel][1024]`.
    synth_fifo: [[f32; 1024]; 2],
}

impl Default for Mp3Decode {
    fn default() -> Self {
        Mp3Decode {
            reservoir: reservoir::Reservoir::default(),
            imdct_overlap: [[0.0; GRANULE_LINES]; 2],
            synth_fifo: [[0.0; 1024]; 2],
        }
    }
}

impl Mp3Decode {
    pub fn new() -> Mp3Decode {
        Mp3Decode::default()
    }

    /// Decode one frame's side-info + main-data into interleaved PCM samples.
    ///
    /// The orchestration below is the wiring diagram; each `*::*` call is a brick
    /// still to be laid (`todo!()`). The public [`crate::Mp3Decoder`] returns
    /// `Unimplemented` until they're built, so this is never reached at runtime.
    pub fn decode_frame(
        &mut self,
        header: &FrameHeader,
        side_info_bytes: &[u8],
        frame_main_data: &[u8],
    ) -> Result<Vec<f32>> {
        let channels = header.channel_mode.channels();
        let granules = header.version.granules();

        // 1. Side information → the per-granule decode recipe.
        let si: SideInfo = sideinfo::parse(header, side_info_bytes)?;

        // 2. Reassemble main data across the reservoir boundary.
        let main = self.reservoir.assemble(si.main_data_begin, frame_main_data);

        // 3..6. Per granule / channel: Huffman → scalefactors → requantize.
        let mut pcm = Vec::with_capacity(granules * GRANULE_LINES * channels);
        let mut bit_pos = 0usize;
        // Granule 0's scalefactors are retained per channel for granule 1 `scfsi`
        // reuse.
        let mut scalefac: [[scalefactors::ScaleFactors; 2]; 2] = Default::default();
        for gr in 0..granules {
            let mut spectrum = GranuleSpectrum::default();
            for ch in 0..channels {
                let gi = &si.granules[gr][ch];
                // part2 (scalefactors) + part3 (Huffman) share one bit budget.
                let part2_3_start = bit_pos;
                let prev = if gr == 1 {
                    Some(scalefac[0][ch].clone())
                } else {
                    None
                };
                let sf =
                    scalefactors::decode(&main, &mut bit_pos, header, &si, gr, ch, prev.as_ref());
                scalefac[gr][ch] = sf.clone();
                let part2_3_end = part2_3_start + gi.part2_3_length as usize;
                let (coeffs, nz) = huffman::decode(&main, &mut bit_pos, part2_3_end, header, gi);
                requantize::apply(header, gi, &sf, &coeffs, nz, &mut spectrum.lines[ch]);
                spectrum.nonzero[ch] = nz;
            }

            // 7. Joint-stereo (MS / intensity) across the two channels.
            stereo::process(header, &si.granules[gr], &mut spectrum);

            // 8..10. Per channel: alias reduction → hybrid IMDCT → synthesis.
            let mut chan_pcm = [[0f32; GRANULE_LINES]; 2];
            for ch in 0..channels {
                antialias::reduce(&si.granules[gr][ch], &mut spectrum.lines[ch]);
                let time = imdct::hybrid(
                    &si.granules[gr][ch],
                    &spectrum.lines[ch],
                    &mut self.imdct_overlap[ch],
                );
                chan_pcm[ch] = synthesis::polyphase(&time, &mut self.synth_fifo[ch]);
            }
            // Interleave the channels into the frame's PCM output.
            for s in 0..GRANULE_LINES {
                for cp in chan_pcm.iter().take(channels) {
                    pcm.push(cp[s]);
                }
            }
        }
        Ok(pcm)
    }
}

/// Entry used by the public decoder once the bricks are in place.
pub fn decode_frame_stub() -> Result<()> {
    Err(Error::Unimplemented("mp3 decode: pipeline not yet built"))
}
