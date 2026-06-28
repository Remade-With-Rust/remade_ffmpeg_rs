//! MP3 encode pipeline (the inverse of decode, plus a psychoacoustic model).
//!
//! ```text
//!  PCM ─▶ analysis filterbank ─▶ MDCT ─▶ psychoacoustic model ─▶
//!  quantize (inner rate loop + outer distortion loop) ─▶ Huffman ─▶
//!  bitstream (side info + main data + reservoir) ─▶ frame
//! ```
//!
//! The psychoacoustic model and the two-loop quantizer are the quality-defining
//! bricks — they decide block type and how to shape quantization noise under the
//! masking threshold, the way LAME does. Everything else is mechanical.

use rff_core::Result;

use crate::bitio::BitWriter;
use crate::decode::scalefactors::ScaleFactors;
use crate::frame::{GranuleSideInfo, SideInfo, GRANULE_LINES};
use crate::header::FrameHeader;

pub mod antialias;
pub mod bitstream;
pub mod fft;
pub mod filterbank;
pub mod huffman;
pub mod mdct;
pub mod psychoacoustic;
pub mod quantize;

/// Persistent encoder state across frames.
pub struct Mp3Encode {
    /// Analysis filterbank FIFO, `[channel][512]`.
    analysis_fifo: [[f32; 512]; 2],
    /// MDCT overlap tail, `[channel][line]`.
    mdct_overlap: [[f32; GRANULE_LINES]; 2],
    /// Encoder-side bit reservoir (spare bits donated to future frames).
    reservoir: bitstream::EncReservoir,
}

impl Default for Mp3Encode {
    fn default() -> Self {
        Mp3Encode {
            analysis_fifo: [[0.0; 512]; 2],
            mdct_overlap: [[0.0; GRANULE_LINES]; 2],
            reservoir: bitstream::EncReservoir::default(),
        }
    }
}

impl Mp3Encode {
    pub fn new() -> Mp3Encode {
        Mp3Encode::default()
    }

    /// Encode one frame of **mono** PCM (`granules × 576` samples) into an MP3
    /// frame: per granule, analysis filterbank → forward MDCT → rate-loop quantize
    /// → Huffman, then assemble (reservoir-free). The dumb-but-valid Floor-3 path.
    pub fn encode_frame(&mut self, header: &FrameHeader, pcm: &[f32]) -> Result<Vec<u8>> {
        let granules = header.version.granules();
        let region_bits = bitstream::region_capacity(header) * 8;
        let budget_per_gr = region_bits / granules;

        let mut side = SideInfo::default();
        let mut main = BitWriter::new();
        for gr in 0..granules {
            let gpcm = &pcm[gr * GRANULE_LINES..];
            let sub = filterbank::analyze(gpcm, &mut self.analysis_fifo[0]);
            let psy = psychoacoustic::analyze(gpcm, header.sample_rate);
            let mut freq = mdct::forward(&sub, psy.block_type, &mut self.mdct_overlap[0]);
            // Forward alias butterflies — the inverse of the decoder's reduce(),
            // which it applies before the IMDCT.
            let block = GranuleSideInfo {
                window_switching: psy.block_type != crate::frame::BlockType::Long,
                block_type: psy.block_type,
                ..Default::default()
            };
            antialias::expand(&block, &mut freq);
            let quant = quantize::loops(header, &freq, &psy, budget_per_gr);

            // Main data per granule: scalefactors (part2) then Huffman (part3).
            side.granules[gr][0] = quant.side.clone();
            let mut sfac = ScaleFactors::default();
            sfac.long.copy_from_slice(&quant.scalefactors[..22]);
            let part2_3_start = main.bit_len();
            bitstream::serialize_scalefactors(&mut main, header, &side, gr, 0, &sfac);
            huffman::encode(&quant, header, &mut main);
            side.granules[gr][0].part2_3_length = (main.bit_len() - part2_3_start) as u16;
        }
        let main_data = main.finish();
        Ok(bitstream::format(
            header,
            &side,
            &main_data,
            &mut self.reservoir,
        ))
    }
}
