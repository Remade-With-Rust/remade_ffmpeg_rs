//! Pure-Rust **AAC-LC decoder + encoder**, no C and no FFI.
//!
//! Extracted from (and the engine of) the
//! [`remade_ffmpeg_rs`](https://github.com/Remade-With-Rust/remade_ffmpeg_rs)
//! project. The crate has **zero dependencies**.
//!
//! - **Decoder**: complete AAC-LC — long/short/transition windows with grouped
//!   scalefactors, all spectral Huffman codebooks, M/S and intensity stereo,
//!   PNS, TNS — verified against FFmpeg (bit-exact on deterministic features).
//!   Entry point: [`AacDecoder`] (handles ADTS framing and raw MP4 access
//!   units alike) or the lower-level [`decode::Decoder`].
//! - **Encoder**: psychoacoustic Bark-scale masking model, two-phase bitrate
//!   rate loop, transient-driven block switching, per-SFB M/S stereo, and a
//!   frame-parallel `encode_stream` (~450× realtime). Entry point:
//!   [`AacEncoder`] with [`AacEncoderConfig`].
//! - **Framing & config**: [`AudioSpecificConfig`] (the MP4 `esds` payload)
//!   and [`AdtsHeader`] (the `.aac` elementary-stream header) parsers plus
//!   their serializers ([`write_audio_specific_config`], [`write_adts_header`],
//!   [`audio_specific_config_bytes`]).
//!
//! The `simd` feature (default) enables runtime-detected AVX2 kernels;
//! `--no-default-features` gives a 100%-safe scalar build. `simd-avx512` adds
//! an opt-in AVX-512 tier.

mod bits;
mod codebook;
pub mod decode;
mod dsp;
pub mod encode;
mod error;
mod huffman;
mod ics;
mod swb;
mod tables;

pub use bits::BitReader;
pub use encode::{
    audio_specific_config_bytes, write_adts_header, write_audio_specific_config, AacEncoder,
    AacEncoderConfig, EncodedPacket,
};
pub use error::{Error, Result};

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
// Decoded PCM — the decoder's native output.
// ---------------------------------------------------------------------------

/// One decoded frame of PCM: interleaved `f32` samples in [-1, 1].
#[derive(Debug, Clone)]
pub struct DecodedAudio {
    pub sample_rate: u32,
    pub channels: u16,
    /// Interleaved samples (`samples.len() = frames × channels`).
    pub samples: Vec<f32>,
    /// The presentation timestamp handed to [`AacDecoder::decode`], if any.
    pub pts: Option<i64>,
}

impl DecodedAudio {
    /// Number of PCM frames (samples per channel).
    pub fn frames(&self) -> usize {
        self.samples.len() / self.channels.max(1) as usize
    }
}

// ---------------------------------------------------------------------------
// Decoder — stream-level: ADTS stripping + lazy core init.
// ---------------------------------------------------------------------------

/// Stream-level AAC-LC decoder: accepts either ADTS frames (`.aac` elementary
/// streams — the config is read from the header) or bare raw access units
/// (MP4 — configure via [`AacDecoder::with_config`] from the `esds`
/// [`AudioSpecificConfig`]).
#[derive(Default)]
pub struct AacDecoder {
    config: Option<AudioSpecificConfig>,
    decoder: Option<decode::Decoder>,
}

impl AacDecoder {
    /// A decoder that learns its parameters from the stream (ADTS headers).
    pub fn new() -> AacDecoder {
        AacDecoder::default()
    }

    /// A decoder pre-configured from an out-of-band [`AudioSpecificConfig`]
    /// (the MP4 `esds` payload) — required for bare raw access units.
    pub fn with_config(cfg: AudioSpecificConfig) -> AacDecoder {
        AacDecoder {
            config: Some(cfg),
            decoder: None,
        }
    }

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

    /// Decode one packet (an ADTS frame or a bare raw access unit) into PCM.
    ///
    /// ADTS framing, if present, is stripped (and configures the decoder on
    /// first use). Returns [`Error::Again`] for an empty packet (nothing to
    /// decode — feed the next one).
    pub fn decode(&mut self, packet: &[u8], pts: Option<i64>) -> Result<DecodedAudio> {
        // Strip ADTS framing if present; MP4 delivers bare access units.
        let mut data = packet;
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
            return Err(Error::Again);
        }
        self.ensure_decoder()?.decode(data, pts)
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

    /// The stream-level decoder learns its config from ADTS and decodes to
    /// interleaved PCM (encoder round-trip; the full-pipeline gates live in
    /// `encode`'s tests).
    #[test]
    fn adts_stream_decodes_via_stream_decoder() {
        let sr = 44100u32;
        let n = 4096usize;
        let pcm: Vec<f32> = (0..n)
            .map(|i| {
                ((i as f64 * 2.0 * std::f64::consts::PI * 440.0 / sr as f64).sin() * 0.5) as f32
            })
            .collect();
        let mut enc = AacEncoder::new(AacEncoderConfig::default());
        enc.push_pcm(&pcm, 1, sr).unwrap();
        enc.finish();
        let mut adts = Vec::new();
        while let Ok(p) = enc.next_packet() {
            let hdr = AdtsHeader {
                object_type: 2,
                sample_rate: sr,
                channels: 1,
                frame_length: 7 + p.data.len(),
                header_len: 7,
            };
            adts.extend_from_slice(&write_adts_header(&hdr));
            adts.extend_from_slice(&p.data);
        }

        let mut dec = AacDecoder::new();
        let mut decoded = 0usize;
        let mut pos = 0usize;
        while pos + 7 <= adts.len() {
            let hdr = parse_adts(&adts[pos..]).unwrap();
            let out = dec
                .decode(&adts[pos..pos + hdr.frame_length], None)
                .unwrap();
            assert_eq!(out.sample_rate, sr);
            assert_eq!(out.channels, 1);
            decoded += out.frames();
            pos += hdr.frame_length;
        }
        assert!(decoded >= n, "decoded fewer samples than encoded");
    }
}
