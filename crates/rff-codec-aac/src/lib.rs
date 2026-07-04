//! In-house **AAC-LC decoder**, pure Rust with no C/FFI.
//!
//! AAC decoding is a multi-stage pipeline; this crate is being built up in
//! stages so each layer is correct and tested before the next is added:
//!
//! 1. **Framing & config** (this layer, done): a bit reader, the
//!    [`AudioSpecificConfig`] (the MP4 `esds` payload) and [`AdtsHeader`]
//!    (the `.aac` elementary-stream header) parsers, and the codec registration.
//! 2. **Syntactic elements**: the `raw_data_block` loop (SCE/CPE/LFE/…) and
//!    `individual_channel_stream` / `ics_info` parsing.
//! 3. **Spectral reconstruction**: Huffman codebooks → quantized coefficients,
//!    inverse quantization with scalefactors, M/S & intensity stereo, TNS.
//! 4. **Synthesis**: IMDCT (2048/256), window (sine/KBD) + overlap-add → PCM.
//!
//! Until stages 2-4 land, [`receive_frame`](rff_codec::Decoder::receive_frame)
//! reports [`Error::Unimplemented`] with a precise message, while configuration
//! and framing are fully functional (so probing AAC tracks already works).

use std::collections::VecDeque;

use rff_codec::{Codec, CodecParams, CodecRegistry, Decoder};
use rff_core::{CodecId, Error, Frame, MediaType, Packet, Result};

mod bits;
mod codebook;
mod decode;
mod dsp;
mod encode;
mod huffman;
mod ics;
mod swb;
mod tables;
pub use bits::BitReader;

/// AAC sample-rate table, indexed by `samplingFrequencyIndex` (ISO 14496-3).
pub const SAMPLE_RATES: [u32; 13] = [
    96000, 88200, 64000, 48000, 44100, 32000, 24000, 22050, 16000, 12000, 11025, 8000, 7350,
];

/// Map a 4-bit sampling-frequency index to a rate (0 for the reserved/escape
/// values 13-15, which require an explicit 24-bit rate).
pub fn sample_rate_for_index(idx: u8) -> u32 {
    SAMPLE_RATES.get(idx as usize).copied().unwrap_or(0)
}

/// Map a sampling rate to its 4-bit index, or None if non-standard (the encoder
/// then uses the 0x0F + explicit-24-bit-rate escape).
pub fn sf_index_for_rate(rate: u32) -> Option<u8> {
    SAMPLE_RATES
        .iter()
        .position(|&r| r == rate)
        .map(|i| i as u8)
}

/// Register the AAC decoder into a [`CodecRegistry`].
pub fn register(registry: &mut CodecRegistry) {
    registry.register(Codec {
        id: CodecId::Aac,
        name: "aac",
        long_name: "AAC (Advanced Audio Coding, Low Complexity)",
        media_type: MediaType::Audio,
        decoder: Some(|| Box::new(AacDecoder::default())),
        encoder: Some(|| Box::new(encode::AacEncoder::new())),
    });
}

// ---------------------------------------------------------------------------
// AudioSpecificConfig — the MP4 `esds` DecoderSpecificInfo (raw AAC config).
// ---------------------------------------------------------------------------

/// Parsed `AudioSpecificConfig` (ISO 14496-3 §1.6.2.1).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct AudioSpecificConfig {
    /// Audio Object Type (2 = AAC-LC, the only one we target).
    pub object_type: u8,
    pub sample_rate: u32,
    /// Channel configuration (1 = mono, 2 = stereo, …).
    pub channels: u16,
}

/// Parse an `AudioSpecificConfig` from its raw bytes (the `esds`/`stsd` config).
pub fn parse_audio_specific_config(data: &[u8]) -> Result<AudioSpecificConfig> {
    let mut r = BitReader::new(data);
    let object_type = read_object_type(&mut r)?;
    let sf_index = r.read_bits(4)? as u8;
    let sample_rate = if sf_index == 0x0F {
        r.read_bits(24)?
    } else {
        sample_rate_for_index(sf_index)
    };
    let channels = r.read_bits(4)? as u16;
    if sample_rate == 0 {
        return Err(Error::invalid("aac: invalid sampling frequency in config"));
    }
    Ok(AudioSpecificConfig {
        object_type,
        sample_rate,
        channels,
    })
}

/// Read the (possibly escaped) 5-bit Audio Object Type.
fn read_object_type(r: &mut BitReader) -> Result<u8> {
    let ot = r.read_bits(5)? as u8;
    if ot == 31 {
        Ok((32 + r.read_bits(6)?) as u8)
    } else {
        Ok(ot)
    }
}

// ---------------------------------------------------------------------------
// ADTS — the `.aac` elementary-stream frame header.
// ---------------------------------------------------------------------------

/// A parsed ADTS frame header (ISO 14496-3 §1.A.2.2.1).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct AdtsHeader {
    /// Audio Object Type (profile + 1).
    pub object_type: u8,
    pub sample_rate: u32,
    pub channels: u16,
    /// Total frame length including this header.
    pub frame_length: usize,
    /// Header size in bytes (7 without CRC, 9 with).
    pub header_len: usize,
}

/// True if `data` begins with an ADTS syncword (0xFFF, layer 00).
pub fn is_adts(data: &[u8]) -> bool {
    data.len() >= 2 && data[0] == 0xFF && (data[1] & 0xF6) == 0xF0
}

/// Parse an ADTS header from the start of `data`.
pub fn parse_adts(data: &[u8]) -> Result<AdtsHeader> {
    if !is_adts(data) || data.len() < 7 {
        return Err(Error::invalid("aac: not an ADTS frame"));
    }
    let mut r = BitReader::new(data);
    r.skip(12)?; // syncword
    r.skip(1)?; // MPEG version
    r.skip(2)?; // layer (00)
    let protection_absent = r.read_bool()?;
    let profile = r.read_bits(2)? as u8; // object_type - 1
    let sf_index = r.read_bits(4)? as u8;
    r.skip(1)?; // private
    let channel_config = r.read_bits(3)? as u16;
    r.skip(4)?; // original/home/copyright id+start
    let frame_length = r.read_bits(13)? as usize;
    // remaining: buffer_fullness(11) + num_raw_data_blocks(2) — not needed here.
    let sample_rate = sample_rate_for_index(sf_index);
    if sample_rate == 0 || frame_length < 7 {
        return Err(Error::invalid("aac: invalid ADTS header"));
    }
    Ok(AdtsHeader {
        object_type: profile + 1,
        sample_rate,
        channels: channel_config,
        frame_length,
        header_len: if protection_absent { 7 } else { 9 },
    })
}

// ---------------------------------------------------------------------------
// Decoder
// ---------------------------------------------------------------------------

#[derive(Default)]
struct AacDecoder {
    config: Option<AudioSpecificConfig>,
    decoder: Option<decode::Decoder>,
    queue: VecDeque<Frame>,
    eof: bool,
}

impl AacDecoder {
    /// Lazily build the stateful decoder once rate/channels are known.
    fn ensure_decoder(&mut self) -> Result<&mut decode::Decoder> {
        if self.decoder.is_none() {
            let cfg = self
                .config
                .ok_or_else(|| Error::invalid("aac: stream parameters unknown"))?;
            self.decoder = Some(decode::Decoder::new(cfg.sample_rate));
        }
        Ok(self.decoder.as_mut().unwrap())
    }
}

impl Decoder for AacDecoder {
    fn configure(&mut self, params: &CodecParams) -> Result<()> {
        // Prefer the out-of-band AudioSpecificConfig (MP4 esds); otherwise fall
        // back to the stream's declared rate/channels (e.g. ADTS streams).
        if !params.extradata.is_empty() {
            self.config = Some(parse_audio_specific_config(&params.extradata)?);
        } else if params.sample_rate > 0 {
            self.config = Some(AudioSpecificConfig {
                object_type: 2,
                sample_rate: params.sample_rate,
                channels: params.channels,
            });
        }
        Ok(())
    }

    fn send_packet(&mut self, packet: &Packet) -> Result<()> {
        // Strip ADTS framing if present; MP4 delivers bare access units.
        let mut data = packet.data.as_slice();
        if is_adts(data) {
            let header = parse_adts(data)?;
            if self.config.is_none() {
                self.config = Some(AudioSpecificConfig {
                    object_type: header.object_type,
                    sample_rate: header.sample_rate,
                    channels: header.channels,
                });
            }
            data = data
                .get(header.header_len..header.frame_length.min(data.len()))
                .unwrap_or(&[]);
        }
        if data.is_empty() {
            return Ok(());
        }
        let pts = packet.pts;
        let au = data.to_vec();
        let frame = self.ensure_decoder()?.decode(&au, pts)?;
        self.queue.push_back(frame);
        Ok(())
    }

    fn receive_frame(&mut self) -> Result<Frame> {
        if let Some(frame) = self.queue.pop_front() {
            return Ok(frame);
        }
        if self.eof {
            Err(Error::Eof)
        } else {
            Err(Error::Again)
        }
    }

    fn flush(&mut self) {
        self.eof = true;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn asc_stereo_44100_aac_lc() {
        // object_type=2 (00010), sf_index=4 (0100)=44100, channels=2 (0010).
        // 00010 0100 0010 → 0001 0010  0001 0000 = 0x12 0x10 (trailing pad).
        let cfg = parse_audio_specific_config(&[0x12, 0x10]).unwrap();
        assert_eq!(cfg.object_type, 2);
        assert_eq!(cfg.sample_rate, 44_100);
        assert_eq!(cfg.channels, 2);
    }

    #[test]
    fn asc_mono_48000() {
        // object_type=2 (00010), sf_index=3 (0011)=48000, channels=1 (0001).
        // 00010 0011 0001 → 0001 0001 1000 1... = 0x11 0x88.
        let cfg = parse_audio_specific_config(&[0x11, 0x88]).unwrap();
        assert_eq!(cfg.sample_rate, 48_000);
        assert_eq!(cfg.channels, 1);
    }

    #[test]
    fn adts_header_parses() {
        // 7-byte ADTS header (no CRC): sync 0xFFF, MPEG-4, layer 0,
        // protection_absent=1, profile 1 (AAC-LC), sf_index 4 (44100),
        // channel_config 2, frame_length 100. Layout verified bit-by-bit.
        let h = [0xFFu8, 0xF1, 0x50, 0x80, 0x0C, 0x9F, 0xFC];
        let hdr = parse_adts(&h).unwrap();
        assert_eq!(hdr.object_type, 2); // profile 1 + 1
        assert_eq!(hdr.sample_rate, 44_100);
        assert_eq!(hdr.channels, 2);
        assert_eq!(hdr.frame_length, 100);
        assert_eq!(hdr.header_len, 7);
    }

    #[test]
    fn is_adts_detects_syncword() {
        assert!(is_adts(&[0xFF, 0xF1, 0x00]));
        assert!(is_adts(&[0xFF, 0xF0, 0x00]));
        assert!(!is_adts(&[0xFF, 0x00]));
        assert!(!is_adts(&[0x00, 0xF1]));
    }

    #[test]
    fn sample_rate_table() {
        assert_eq!(sample_rate_for_index(3), 48_000);
        assert_eq!(sample_rate_for_index(4), 44_100);
        assert_eq!(sample_rate_for_index(8), 16_000);
        assert_eq!(sample_rate_for_index(15), 0); // escape value
    }
}
