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

use rff_core::{Error, Result};

use crate::bitio::BitWriter;
use crate::frame::{SideInfo, GRANULE_LINES};
use crate::header::FrameHeader;

pub mod bitstream;
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

    /// Encode one frame of interleaved PCM into an MP3 frame. The orchestration
    /// is the wiring diagram; each stage is a brick still to be laid.
    pub fn encode_frame(&mut self, header: &FrameHeader, pcm: &[f32]) -> Result<Vec<u8>> {
        let channels = header.channel_mode.channels();
        let granules = header.version.granules();
        let bit_budget = header.frame_size() * 8;

        let mut writer = BitWriter::new();
        let mut side = SideInfo::default();
        for gr in 0..granules {
            for ch in 0..channels {
                let sub = filterbank::analyze(pcm, &mut self.analysis_fifo[ch]);
                let psy = psychoacoustic::analyze(pcm);
                let freq = mdct::forward(&sub, psy.block_type, &mut self.mdct_overlap[ch]);
                let quant = quantize::loops(&freq, &psy, bit_budget / (granules * channels));
                huffman::encode(&quant, header, &mut writer);
                side.granules[gr][ch] = quant.side.clone();
            }
        }
        let main_data = writer.finish();
        let frame = bitstream::format(header, &side, &main_data, &mut self.reservoir);
        Ok(frame)
    }
}

/// Entry used by the public encoder once the bricks are in place.
pub fn encode_frame_stub() -> Result<()> {
    Err(Error::Unimplemented("mp3 encode: pipeline not yet built"))
}
