//! Pure-Rust **AAC decoder** (LC, Main, LTP, HE-AAC v1/v2, ER LC/LTP/LD/ELD)
//! **+ AAC-LC encoder**, no C and no FFI.
//!
//! Extracted from (and the engine of) the
//! [`remade_ffmpeg_rs`](https://github.com/Remade-With-Rust/remade_ffmpeg_rs)
//! project. The crate has **zero dependencies**.
//!
//! - **Decoder**: the MPEG-4 AAC family short of USAC — LC, Main (prediction),
//!   LTP, HE-AAC v1 (SBR) and v2 (Parametric Stereo), ER AAC-LC/LTP, AAC-LD and
//!   AAC-ELD; 1024/960-sample frames, mono to 7.1 and PCE layouts, coupling
//!   channels. Sample-exact (≤1 LSB) against FFmpeg on every non-USAC
//!   ISO/IEC 14496-26 conformance stream (two up to documented FFmpeg
//!   deviations). Entry point: [`AacDecoder`] (ADTS framing and raw MP4 access
//!   units alike), [`latm::LatmDecoder`] for LOAS, or the lower-level
//!   [`decode::Decoder`] driven by a full [`config::StreamConfig`].
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
/// The full `AudioSpecificConfig` (GA/ELD/PCE/SBR/PS) — what a stream is.
pub mod config;
pub mod decode;
mod dsp;
pub mod encode;
mod error;
mod huffman;
mod ics;
/// The quality lab (feature `lab`): deterministic corpus, the NMR metric, and the
/// bitrate-ladder runner. This is the verdict instrument for the Great Gate
/// campaign — see `docs/finished/codec-aac-great-gate.md`.
#[cfg(feature = "lab")]
pub mod lab;
/// LATM/LOAS transport (ISO 14496-3 §1.7) — the MPEG-TS / broadcast carriage
/// format, alongside ADTS (`.aac`) and MP4 `esds`.
pub mod latm;
/// Stage profiler + SIMD-arm census (feature `profile`; no-ops without it).
pub mod prof;
/// HE-AAC (SBR / Parametric Stereo): signalling, capability reporting, and the
/// reconstruction itself. See the module docs for exactly what is implemented.
pub mod sbr;
mod swb;
mod tables;
#[rustfmt::skip] // generated data
mod tables_ext;

pub use bits::BitReader;
pub use encode::{
    audio_specific_config_bytes, write_adts_header, write_audio_specific_config, AacEncoder,
    AacEncoderConfig, EncodedPacket, WindowShape,
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
    stream: Option<config::StreamConfig>,
    decoder: Option<decode::Decoder>,
    /// HE-AAC parameters, when built via [`AacDecoder::with_config_bytes`].
    sbr: Option<sbr::SbrConfig>,
    /// (object type, rate, channel configuration) of the ADTS stream decoded so
    /// far: a change mid-stream rebuilds the decoder, as a real stream switch must.
    adts_key: Option<(u8, u32, u16)>,
}

impl AacDecoder {
    /// A decoder that learns its parameters from the stream (ADTS headers).
    pub fn new() -> Self {
        Self::default()
    }

    /// A decoder pre-configured from an out-of-band [`AudioSpecificConfig`]
    /// (the MP4 `esds` payload) — required for bare raw access units.
    pub fn with_config(cfg: AudioSpecificConfig) -> Self {
        Self {
            stream: Some(config::StreamConfig::lc(
                cfg.object_type,
                cfg.sample_rate,
                cfg.channels as u8,
            )),
            config: Some(cfg),
            ..Default::default()
        }
    }

    /// A decoder configured from the **raw** `AudioSpecificConfig` bytes.
    ///
    /// Prefer this over [`with_config`](Self::with_config) whenever the raw bytes
    /// are available (they always are — `esds`, `StreamMuxConfig`): HE-AAC
    /// signalling, PCE channel layouts, the 960-sample frame flag and the
    /// LD/ELD parameters all live in fields the plain [`AudioSpecificConfig`]
    /// does not carry.
    pub fn with_config_bytes(data: &[u8]) -> Result<Self> {
        let stream = config::parse(data)?;
        Ok(Self::with_stream_config(stream))
    }

    /// A decoder for an already-parsed [`config::StreamConfig`].
    pub fn with_stream_config(stream: config::StreamConfig) -> Self {
        let sbr = sbr::SbrConfig {
            sbr_present: stream.sbr,
            ps_present: stream.ps,
            core_sample_rate: stream.sample_rate,
            // ELD's low-delay SBR is not reconstructed: output stays at the core rate.
            output_sample_rate: if stream.sbr && stream.object_type != config::aot::ER_AAC_ELD {
                stream.ext_sample_rate
            } else {
                stream.sample_rate
            },
            core_object_type: stream.object_type,
        };
        Self {
            config: Some(AudioSpecificConfig {
                object_type: stream.object_type,
                sample_rate: stream.sample_rate,
                channels: stream.channels() as u16,
            }),
            stream: Some(stream),
            decoder: None,
            sbr: Some(sbr),
            adts_key: None,
        }
    }

    /// HE-AAC parameters, when the decoder was built from raw config bytes.
    pub fn sbr_config(&self) -> Option<sbr::SbrConfig> {
        self.sbr
    }

    /// How much of this stream can actually be reconstructed.
    pub fn sbr_support(&self) -> sbr::SbrSupport {
        match self.sbr {
            Some(s) => s.support(),
            None => sbr::SbrSupport::NotPresent,
        }
    }

    /// The stream's **output** sample rate (twice the coded rate for dual-rate
    /// HE-AAC). Returns `None` until the configuration is known.
    pub fn output_sample_rate(&self) -> Option<u32> {
        if let Some(d) = &self.decoder {
            return Some(d.output_sample_rate());
        }
        match self.sbr {
            Some(s) if s.sbr_present => Some(s.output_sample_rate),
            _ => self.config.map(|c| c.sample_rate),
        }
    }

    fn ensure_decoder(&mut self) -> Result<&mut decode::Decoder> {
        if self.decoder.is_none() {
            let stream = self
                .stream
                .clone()
                .ok_or_else(|| Error::invalid("aac: stream parameters unknown"))?;
            self.decoder = Some(decode::Decoder::with_stream_config(stream)?);
        }
        Ok(self.decoder.as_mut().unwrap())
    }

    /// Decode one packet (an ADTS frame or a bare raw access unit) into PCM.
    ///
    /// ADTS framing, if present, is stripped (and configures the decoder; a
    /// header change mid-stream reconfigures it). An ADTS frame carrying several
    /// raw data blocks decodes to all of them. Returns [`Error::Again`] for an
    /// empty packet (nothing to decode — feed the next one).
    pub fn decode(&mut self, packet: &[u8], pts: Option<i64>) -> Result<DecodedAudio> {
        if !is_adts(packet) {
            if packet.is_empty() {
                return Err(Error::Again);
            }
            return self.ensure_decoder()?.decode(packet, pts);
        }
        let header = parse_adts(packet)?;
        let key = (header.object_type, header.sample_rate, header.channels);
        if self.adts_key != Some(key) {
            let reconfigure = self.adts_key.is_some() || self.stream.is_none();
            self.adts_key = Some(key);
            if reconfigure {
                self.config = Some(AudioSpecificConfig {
                    object_type: header.object_type,
                    sample_rate: header.sample_rate,
                    channels: header.channels,
                });
                self.stream = Some(config::StreamConfig::lc(
                    header.object_type,
                    header.sample_rate,
                    header.channels as u8,
                ));
                self.decoder = None;
            }
        }
        let end = header.frame_length.min(packet.len());
        let blocks = adts_raw_blocks(packet) as usize;
        let protected = header.header_len == 9;
        if blocks == 0 {
            let data = packet.get(header.header_len..end).unwrap_or(&[]);
            if data.is_empty() {
                return Err(Error::Again);
            }
            return self.ensure_decoder()?.decode(data, pts);
        }
        // Several raw_data_blocks: with protection the header carries their
        // positions (and each block a CRC); without, they simply follow each
        // other, each byte-aligned.
        let mut pos = if protected { 7 + 2 * blocks + 2 } else { 7 };
        let mut out: Option<DecodedAudio> = None;
        for _ in 0..=blocks {
            let data = packet.get(pos..end).unwrap_or(&[]);
            if data.is_empty() {
                break;
            }
            let (frame, used) = self.ensure_decoder()?.decode_block(data, pts)?;
            pos += used + if protected { 2 } else { 0 };
            match out.as_mut() {
                Some(o) => o.samples.extend_from_slice(&frame.samples),
                None => out = Some(frame),
            }
        }
        out.ok_or(Error::Again)
    }
}

/// `number_of_raw_data_blocks_in_frame` of an ADTS header (0 = one block).
pub fn adts_raw_blocks(data: &[u8]) -> u8 {
    data.get(6).map(|b| b & 0x03).unwrap_or(0)
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
                ((i as f64 * 2.0 * std::f64::consts::PI * 440.0 / f64::from(sr)).sin() * 0.5) as f32
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
