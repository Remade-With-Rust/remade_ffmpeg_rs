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

/// Lightweight stage profiler (near-zero cost; read via [`prof::dump`]). Used to
/// find the real encode hotspots instead of guessing — the basis of the
/// optimization plan's measurements.
pub mod prof {
    use std::sync::atomic::{AtomicU64, Ordering};
    use std::time::Instant;

    pub static FILTERBANK: AtomicU64 = AtomicU64::new(0);
    pub static PSY: AtomicU64 = AtomicU64::new(0);
    pub static MDCT: AtomicU64 = AtomicU64::new(0);
    pub static QUANT: AtomicU64 = AtomicU64::new(0);
    pub static HUFF: AtomicU64 = AtomicU64::new(0);
    // Block-type distribution (the split decides which quantizer path runs).
    pub static N_LONG: AtomicU64 = AtomicU64::new(0);
    pub static N_SHORT: AtomicU64 = AtomicU64::new(0);

    /// Time `f` into `bucket` (nanoseconds, summed across calls).
    #[inline]
    pub fn time<T>(bucket: &AtomicU64, f: impl FnOnce() -> T) -> T {
        let t = Instant::now();
        let r = f();
        bucket.fetch_add(t.elapsed().as_nanos() as u64, Ordering::Relaxed);
        r
    }

    /// Print the per-stage breakdown and reset the counters.
    pub fn dump() {
        let stages = [
            ("filterbank", &FILTERBANK),
            ("psychoacoustic", &PSY),
            ("mdct+alias", &MDCT),
            ("quantize", &QUANT),
            ("huffman-emit", &HUFF),
        ];
        let total: u64 = stages.iter().map(|(_, b)| b.load(Ordering::Relaxed)).sum();
        eprintln!(
            "--- encode stage profile (total {:.1} ms) ---",
            total as f64 / 1e6
        );
        for (name, b) in stages {
            let ns = b.swap(0, Ordering::Relaxed);
            eprintln!(
                "  {name:<16} {:>8.1} ms  {:>5.1}%",
                ns as f64 / 1e6,
                100.0 * ns as f64 / total.max(1) as f64
            );
        }
        let (nl, ns) = (
            N_LONG.swap(0, Ordering::Relaxed),
            N_SHORT.swap(0, Ordering::Relaxed),
        );
        eprintln!("  granules: {nl} long, {ns} short");
    }
}

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
                let sub = prof::time(&prof::FILTERBANK, || {
                    filterbank::analyze(gpcm, &mut self.analysis_fifo[ch])
                });
                let mut psy = prof::time(&prof::PSY, || {
                    psychoacoustic::analyze(gpcm, fheader.sample_rate)
                });
                // MPEG-2/2.5 uses flat scalefactors (the LSF scalefactor scheme is
                // not yet coded), so disable the distortion loop's shaping there.
                if !matches!(fheader.version, crate::header::MpegVersion::V1) {
                    psy.thresholds = [f32::MAX; crate::frame::SFB_LONG];
                }
                let block = GranuleSideInfo {
                    window_switching: bt != BlockType::Long,
                    block_type: bt,
                    ..Default::default()
                };
                let freq = prof::time(&prof::MDCT, || {
                    let mut freq = mdct::forward(&sub, bt, &mut self.mdct_overlap[ch]);
                    // Forward alias butterflies — the inverse of the decoder's reduce()
                    // (applied for long/start/stop; pure short blocks skip it).
                    antialias::expand(&block, &mut freq);
                    freq
                });

                let quant = prof::time(&prof::QUANT, || {
                    if bt == BlockType::Short {
                        prof::N_SHORT.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
                        // Reorder to bitstream order, then the short quantizer.
                        let fbs =
                            shortblock::reorder_subband_to_bitstream(fheader.sample_rate, &freq);
                        shortblock::quantize_short(&fheader, &fbs, budget)
                    } else {
                        prof::N_LONG.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
                        // Long/start/stop: the quantizer uses the right regions for the
                        // block type, so the cost it budgets matches the emit.
                        match quality {
                            Some(target) => quantize::loops_vbr(&fheader, &freq, &psy, target, bt),
                            None => quantize::loops(&fheader, &freq, &psy, budget, bt),
                        }
                    }
                });

                // Main data per granule/channel: scalefactors (part2) then Huffman.
                // Long/start/stop carry long scalefactors; short uses flat (zero).
                side.granules[gr][ch] = quant.side.clone();
                let mut sfac = ScaleFactors::default();
                if bt != BlockType::Short {
                    sfac.long.copy_from_slice(&quant.scalefactors[..22]);
                }
                let part2_3_start = main.bit_len();
                prof::time(&prof::HUFF, || {
                    bitstream::serialize_scalefactors(&mut main, &fheader, &side, gr, ch, &sfac);
                    huffman::encode(&quant, &fheader, &mut main);
                });
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

#[cfg(test)]
mod profile_tests {
    use super::*;
    use crate::frame::ChannelMode;
    use crate::header::{FrameHeader, MpegVersion};

    /// Profiling driver (run explicitly): encodes ~10 s of *dense* broadband audio
    /// — the case that actually stresses the two-loop quantizer — and prints the
    /// per-stage breakdown. `cargo test -p rff-codec-mp3 profile_encode_dense --
    /// --ignored --nocapture`.
    #[test]
    #[ignore]
    fn profile_encode_dense() {
        let header = FrameHeader {
            version: MpegVersion::V1,
            crc_protected: false,
            bitrate_kbps: 128,
            sample_rate: 44100,
            padding: false,
            channel_mode: ChannelMode::Stereo,
            copyright: false,
            original: true,
            emphasis: 0,
        };
        let frames = 380; // ~10 s at 1152 samples/frame
        let per_ch = 2 * GRANULE_LINES; // V1: 2 granules/frame
        let mut enc = Mp3Encode::new();
        let mut s = 0x2545_F491u32;
        let mut rng = || {
            s = s.wrapping_mul(1_664_525).wrapping_add(1_013_904_223);
            (s >> 8) as f32 / (1u32 << 24) as f32 * 2.0 - 1.0
        };
        // Harmonically dense but *smooth* (no transients) → long blocks, the path
        // real music spends most of its time in and the two-loop quantizer's home.
        let mut n = 0usize;
        for _ in 0..frames {
            let mut l = vec![0f32; per_ch];
            let mut r = vec![0f32; per_ch];
            for i in 0..per_ch {
                let t = n as f32 / 44100.0;
                let mut v = 0f32;
                for k in 1..=28 {
                    v += (1.0 / k as f32)
                        * (2.0 * std::f32::consts::PI * 110.0 * k as f32 * t).sin();
                }
                l[i] = (0.25 * v + 0.02 * rng()).clamp(-1.0, 1.0);
                r[i] = (0.25 * v + 0.02 * rng()).clamp(-1.0, 1.0); // slight decorrelation
                n += 1;
            }
            enc.encode_frame(&header, &[l, r], None).unwrap();
        }
        eprintln!("[profile] {frames} frames (~10 s, harmonic-dense stereo, 128k CBR)");
        prof::dump();
    }
}
