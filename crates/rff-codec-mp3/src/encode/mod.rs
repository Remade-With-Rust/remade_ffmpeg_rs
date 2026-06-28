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
use crate::frame::{BlockType, GranuleSideInfo, SideInfo, GRANULE_LINES};
use crate::header::FrameHeader;

pub mod antialias;
pub mod bitstream;
pub mod fft;
pub mod filterbank;
pub mod huffman;
pub mod mdct;
pub mod psychoacoustic;
pub mod quantize;
pub mod shortblock;
pub mod stereo;

/// Persistent encoder state across frames.
pub struct Mp3Encode {
    /// Analysis filterbank FIFO, `[channel][512]`.
    analysis_fifo: [[f32; 512]; 2],
    /// MDCT overlap tail, `[channel][line]`.
    mdct_overlap: [[f32; GRANULE_LINES]; 2],
    /// Encoder-side bit reservoir (spare bits donated to future frames).
    reservoir: bitstream::EncReservoir,
    /// Last granule's window type — the block-switch FSM carries it across frames.
    prev_block_type: crate::frame::BlockType,
}

impl Default for Mp3Encode {
    fn default() -> Self {
        Mp3Encode {
            analysis_fifo: [[0.0; 512]; 2],
            mdct_overlap: [[0.0; GRANULE_LINES]; 2],
            reservoir: bitstream::EncReservoir::default(),
            prev_block_type: crate::frame::BlockType::Long,
        }
    }
}

impl Mp3Encode {
    pub fn new() -> Mp3Encode {
        Mp3Encode::default()
    }

    /// Encode one frame from per-channel PCM (`channels[ch]` holds this frame's
    /// `granules × 576` samples): per granule, per channel, analysis filterbank →
    /// forward MDCT → rate-loop quantize → Huffman, then assemble (reservoir-free).
    ///
    /// Mono, independent stereo, or **mid/side joint stereo** (chosen per frame
    /// when the channels are correlated). Main data is laid out granule-major then
    /// channel-major, the order the decoder reads it.
    ///
    /// `quality` selects the rate mode: `None` is CBR (the header's bitrate);
    /// `Some(target_nmr)` is **VBR** — each granule quantizes to that quality and
    /// the frame's bitrate is picked to fit the result.
    pub fn encode_frame(
        &mut self,
        header: &FrameHeader,
        channels: &[Vec<f32>],
        quality: Option<f32>,
    ) -> Result<Vec<u8>> {
        let nch = header.channel_mode.channels();
        let granules = header.version.granules();

        // Per-frame M/S decision (stereo only). M/S is exact in the PCM domain
        // because the filterbank/MDCT are linear: storing M=(L+R)/√2, S=(L−R)/√2
        // and letting the decoder rotate back reconstructs L/R. Only worth it when
        // the channels are correlated (S small → cheap to code).
        let use_ms = nch == 2 && stereo::prefer_mid_side(&channels[0], &channels[1]);
        let coded = if use_ms {
            stereo::mid_side(&channels[0], &channels[1])
        } else {
            channels.to_vec()
        };
        let mut fheader = header.clone();
        if use_ms {
            fheader.channel_mode = crate::frame::ChannelMode::JointStereo {
                ms_stereo: true,
                intensity_stereo: false,
            };
        }

        // Block-switch decision (Q5): a transient in either channel of a granule
        // triggers short blocks, bracketed by the required start/stop windows.
        let attacks: Vec<bool> = (0..granules)
            .map(|gr| {
                (0..nch).any(|ch| {
                    let g = &coded[ch][gr * GRANULE_LINES..(gr + 1) * GRANULE_LINES];
                    psychoacoustic::detect_attack(g)
                })
            })
            .collect();
        let (block_types, new_prev) =
            shortblock::decide_block_types(self.prev_block_type, &attacks);
        self.prev_block_type = new_prev;

        let budget = (bitstream::region_capacity(&fheader) * 8) / (granules * nch);
        let mut side = SideInfo::default();
        let mut main = BitWriter::new();
        for gr in 0..granules {
            let bt = block_types[gr];
            for ch in 0..nch {
                let gpcm = &coded[ch][gr * GRANULE_LINES..];
                let sub = filterbank::analyze(gpcm, &mut self.analysis_fifo[ch]);
                let mut psy = psychoacoustic::analyze(gpcm, fheader.sample_rate);
                // MPEG-2/2.5 uses flat scalefactors (the LSF scalefactor scheme is
                // not yet coded), so disable the distortion loop's shaping there.
                if !matches!(fheader.version, crate::header::MpegVersion::V1) {
                    psy.thresholds = [f32::MAX; crate::frame::SFB_LONG];
                }
                let mut freq = mdct::forward(&sub, bt, &mut self.mdct_overlap[ch]);
                // Forward alias butterflies — the inverse of the decoder's reduce()
                // (applied for long/start/stop; pure short blocks skip it).
                let block = GranuleSideInfo {
                    window_switching: bt != BlockType::Long,
                    block_type: bt,
                    ..Default::default()
                };
                antialias::expand(&block, &mut freq);

                let quant = if bt == BlockType::Short {
                    // Reorder to bitstream order, then the short quantizer.
                    let fbs = shortblock::reorder_subband_to_bitstream(fheader.sample_rate, &freq);
                    shortblock::quantize_short(&fheader, &fbs, budget)
                } else {
                    // Long/start/stop: the quantizer uses the right regions for the
                    // block type, so the cost it budgets matches the emit.
                    match quality {
                        Some(target) => quantize::loops_vbr(&fheader, &freq, &psy, target, bt),
                        None => quantize::loops(&fheader, &freq, &psy, budget, bt),
                    }
                };

                // Main data per granule/channel: scalefactors (part2) then Huffman.
                // Long/start/stop carry long scalefactors; short uses flat (zero).
                side.granules[gr][ch] = quant.side.clone();
                let mut sfac = ScaleFactors::default();
                if bt != BlockType::Short {
                    sfac.long.copy_from_slice(&quant.scalefactors[..22]);
                }
                let part2_3_start = main.bit_len();
                bitstream::serialize_scalefactors(&mut main, &fheader, &side, gr, ch, &sfac);
                huffman::encode(&quant, &fheader, &mut main);
                side.granules[gr][ch].part2_3_length = (main.bit_len() - part2_3_start) as u16;
            }
        }
        let main_data = main.finish();

        // VBR: size the frame to its actual main data (CBR keeps the fixed rate).
        if quality.is_some() {
            fheader.bitrate_kbps = bitstream::smallest_bitrate_for(&fheader, main_data.len());
        }
        Ok(bitstream::format(
            &fheader,
            &side,
            &main_data,
            &mut self.reservoir,
        ))
    }
}
