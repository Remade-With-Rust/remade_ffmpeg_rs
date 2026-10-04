//! **LATM / LOAS** — the low-overhead transport (ISO 14496-3 §1.7).
//!
//! ADTS carries AAC in `.aac` files; MP4 carries it in `esds`. Broadcast and
//! MPEG-TS carry it in **LOAS** (Low Overhead Audio Stream), whose payload is a
//! **LATM** (Low Overhead Audio Transport Multiplex) `AudioMuxElement`. Without
//! it, an AAC decoder cannot read a TS stream at all — which matters here because
//! this workspace ships `rff-format-ts`.
//!
//! The layering, outermost first:
//!
//! ```text
//! AudioSyncStream            syncword 0x2B7 (11b) + audioMuxLengthBytes (13b)
//!   AudioMuxElement          muxConfigPresent = 1 (the LOAS case)
//!     useSameStreamMux (1b)  0 -> a StreamMuxConfig follows
//!     StreamMuxConfig        audioMuxVersion, framing flags, AudioSpecificConfig
//!     PayloadLengthInfo      the AU length, as a chain of 8-bit values
//!     PayloadMux             the raw_data_block bytes
//! ```
//!
//! # Scope
//!
//! `audioMuxVersion` 0 and 1, every AAC `frameLengthType` (0 byte-counted, 1
//! fixed), other-data and CRC fields, and mid-stream configuration changes (see
//! [`LatmDecoder`]). A single program and layer, which is what broadcast uses
//! and what the reference decoder supports; multiple programs/layers are
//! refused rather than mis-parsed.

use crate::bits::BitReader;
use crate::config::StreamConfig;
use crate::encode::BitWriter;
use crate::{AacDecoder, AudioSpecificConfig, DecodedAudio, Error, Result};

/// The 11-bit LOAS sync pattern.
const LOAS_SYNC: u32 = 0x2B7;
/// `AudioSyncStream` header size in bytes (11-bit sync + 13-bit length = 24 bits).
pub const LOAS_HEADER_LEN: usize = 3;

/// One parsed LOAS frame.
#[derive(Debug, Clone)]
pub struct LoasFrame {
    /// Stream parameters from the embedded `AudioSpecificConfig` (simple view).
    pub config: AudioSpecificConfig,
    /// The full configuration (SBR/PS signalling, PCE, frame length, ...).
    pub stream: StreamConfig,
    /// The raw access unit (a `raw_data_block`), ready for `decode::Decoder`.
    pub au: Vec<u8>,
    /// Total bytes consumed, so a caller can walk a stream.
    pub frame_length: usize,
}

/// Does `data` start with a LOAS `audioSyncStream` header?
pub fn is_loas(data: &[u8]) -> bool {
    data.len() >= LOAS_HEADER_LEN && data[0] == 0x56 && (data[1] & 0xE0) == 0xE0
}

/// Byte offset of the next LOAS syncword, for resynchronising a damaged stream.
pub fn find_sync(data: &[u8]) -> Option<usize> {
    (0..data.len().saturating_sub(LOAS_HEADER_LEN)).find(|&i| is_loas(&data[i..]))
}

/// `LatmGetValue()`: a 2-bit byte count then that many + 1 bytes.
fn latm_get_value(r: &mut BitReader) -> Result<u32> {
    let n = r.read_bits(2)?;
    let mut v = 0u32;
    for _ in 0..=n {
        v = (v << 8) | r.read_bits(8)?;
    }
    Ok(v)
}

/// The simple three-field view of a full stream configuration.
fn simple_view(c: &StreamConfig) -> AudioSpecificConfig {
    AudioSpecificConfig {
        object_type: c.object_type,
        sample_rate: c.sample_rate,
        channels: c.channels() as u16,
    }
}

/// Mux state carried between frames by `useSameStreamMux`.
#[derive(Debug, Clone)]
struct MuxConfig {
    stream: StreamConfig,
    frame_length_type: u32,
    frame_length: usize,
}

/// Parse `StreamMuxConfig`: audioMuxVersion 0 and 1, one program and layer
/// (the reference decoder's coverage; multiple programs/layers are refused).
fn read_stream_mux_config(r: &mut BitReader) -> Result<MuxConfig> {
    let audio_mux_version = r.read_bool()?;
    let version_a = if audio_mux_version {
        r.read_bool()?
    } else {
        false
    };
    if version_a {
        return Err(Error::unsupported("latm: audioMuxVersionA = 1 is reserved"));
    }
    if audio_mux_version {
        let _tara_buffer_fullness = latm_get_value(r)?;
    }
    let _all_streams_same_time_framing = r.read_bool()?;
    let _num_sub_frames = r.read_bits(6)?;
    if r.read_bits(4)? != 0 {
        return Err(Error::unsupported("latm: multiple programs not supported"));
    }
    if r.read_bits(3)? != 0 {
        return Err(Error::unsupported("latm: multiple layers not supported"));
    }
    let stream = if !audio_mux_version {
        let start = r.position();
        crate::config::parse_from_opts(r, start, false)?
    } else {
        // Bound the config to ascLen bits so the sync-extension probe cannot run
        // past it, then consume exactly ascLen (fill bits included).
        let asc_len = latm_get_value(r)? as usize;
        let n = asc_len.min(r.bits_left());
        let mut bytes = Vec::with_capacity(n.div_ceil(8));
        let mut left = n;
        while left >= 8 {
            bytes.push(r.read_bits(8)? as u8);
            left -= 8;
        }
        if left > 0 {
            bytes.push((r.read_bits(left as u32)? << (8 - left)) as u8);
        }
        let mut sub = BitReader::new(&bytes);
        crate::config::parse_from_opts(&mut sub, 0, true)?
    };
    let frame_length_type = r.read_bits(3)?;
    let mut frame_length = 0usize;
    match frame_length_type {
        0 => {
            let _latm_buffer_fullness = r.read_bits(8)?;
        }
        1 => frame_length = r.read_bits(9)? as usize,
        3..=5 => r.skip(6)?,
        6 | 7 => r.skip(1)?,
        _ => return Err(Error::invalid("latm: reserved frameLengthType")),
    }
    if r.read_bool()? {
        if audio_mux_version {
            let _other_data_len_bits = latm_get_value(r)?;
        } else {
            loop {
                let esc = r.read_bool()?;
                r.skip(8)?;
                if !esc {
                    break;
                }
            }
        }
    }
    if r.read_bool()? {
        let _crc = r.read_bits(8)?; // crcCheckSum
    }
    Ok(MuxConfig {
        stream,
        frame_length_type,
        frame_length,
    })
}

/// Parse one LOAS frame starting at `data[0]`.
///
/// `muxConfigPresent` is 1 by definition inside an `audioSyncStream`, so a
/// `StreamMuxConfig` is expected unless `useSameStreamMux` says to reuse the
/// previous one — which this function cannot do, since it is stateless. Callers
/// walking a stream should use [`LatmReader`], which carries that state.
pub fn parse_loas_frame(data: &[u8]) -> Result<LoasFrame> {
    LatmReader::new().parse(data)
}

/// A stateful LOAS reader.
///
/// Real streams send the `StreamMuxConfig` on the first frame and then set
/// `useSameStreamMux = 1` for a long run of frames afterwards — that is the whole
/// point of "low overhead". A stateless parser therefore fails on the *majority*
/// of frames in any real capture, which is why this type exists.
#[derive(Debug, Default, Clone)]
pub struct LatmReader {
    mux: Option<MuxConfig>,
}

impl LatmReader {
    pub fn new() -> LatmReader {
        LatmReader::default()
    }

    /// The most recently seen stream configuration, if any.
    pub fn config(&self) -> Option<AudioSpecificConfig> {
        self.mux.as_ref().map(|m| simple_view(&m.stream))
    }

    /// The most recently seen FULL stream configuration (SBR/PS, PCE, ...).
    pub fn stream_config(&self) -> Option<&StreamConfig> {
        self.mux.as_ref().map(|m| &m.stream)
    }

    /// Parse one LOAS frame, remembering its `StreamMuxConfig` for later frames.
    pub fn parse(&mut self, data: &[u8]) -> Result<LoasFrame> {
        if !is_loas(data) {
            return Err(Error::invalid("latm: no audioSyncStream syncword"));
        }
        let mut r = BitReader::new(data);
        let sync = r.read_bits(11)?;
        debug_assert_eq!(sync, LOAS_SYNC);
        let mux_len = r.read_bits(13)? as usize;
        let frame_length = LOAS_HEADER_LEN + mux_len;
        if data.len() < frame_length {
            return Err(Error::Again);
        }
        let mut r = BitReader::new(&data[..frame_length]);
        r.skip(24)?;

        // AudioMuxElement(muxConfigPresent = 1)
        let use_same = r.read_bool()?;
        if !use_same {
            self.mux = Some(read_stream_mux_config(&mut r)?);
        }
        let mux = self
            .mux
            .clone()
            .ok_or_else(|| Error::invalid("latm: useSameStreamMux before any config"))?;

        // PayloadLengthInfo
        let au_len = match mux.frame_length_type {
            0 => {
                let mut n = 0usize;
                loop {
                    let v = r.read_bits(8)? as usize;
                    n += v;
                    if v != 255 {
                        break;
                    }
                }
                n
            }
            1 => mux.frame_length,
            3 | 5 | 7 => {
                r.skip(2)?;
                0
            }
            _ => 0,
        };
        if au_len * 8 > r.bits_left() {
            return Err(Error::invalid("latm: incomplete frame"));
        }

        // PayloadMux — au_len bytes, still bit-aligned to the reader position.
        let mut au = Vec::with_capacity(au_len);
        for _ in 0..au_len {
            au.push(r.read_bits(8)? as u8);
        }

        Ok(LoasFrame {
            config: simple_view(&mux.stream),
            stream: mux.stream,
            au,
            frame_length,
        })
    }
}

/// LOAS in, PCM out: a [`LatmReader`] driving an [`AacDecoder`], rebuilt
/// whenever the stream's configuration changes mid-stream (a broadcast service
/// switching from stereo to 5.1, say).
#[derive(Default)]
pub struct LatmDecoder {
    reader: LatmReader,
    dec: Option<(StreamConfig, AacDecoder)>,
}

impl LatmDecoder {
    pub fn new() -> LatmDecoder {
        LatmDecoder::default()
    }

    /// Decode the LOAS frame at the start of `data`; returns the PCM and the
    /// number of bytes consumed.
    pub fn decode(&mut self, data: &[u8], pts: Option<i64>) -> Result<(DecodedAudio, usize)> {
        let frame = self.reader.parse(data)?;
        let rebuild = match &self.dec {
            Some((cfg, _)) => *cfg != frame.stream,
            None => true,
        };
        if rebuild {
            self.dec = Some((
                frame.stream.clone(),
                AacDecoder::with_stream_config(frame.stream.clone()),
            ));
        }
        let out = self.dec.as_mut().unwrap().1.decode(&frame.au, pts)?;
        Ok((out, frame.frame_length))
    }
}

/// Serialize one LOAS frame carrying `au`, with a full `StreamMuxConfig`.
///
/// Every frame gets its own config (`useSameStreamMux = 0`). That costs ~3 bytes
/// per frame versus the minimum, and buys random access: a receiver can start
/// decoding at any frame, which is what broadcast actually needs.
pub fn write_loas_frame(cfg: &AudioSpecificConfig, au: &[u8]) -> Vec<u8> {
    let mut w = BitWriter::new();
    // --- AudioMuxElement(muxConfigPresent = 1) ---
    w.write_bool(false); // useSameStreamMux = 0 -> StreamMuxConfig follows

    // --- StreamMuxConfig ---
    w.write_bool(false); // audioMuxVersion = 0
    w.write_bool(true); // allStreamsSameTimeFraming = 1
    w.write(0, 6); // numSubFrames = 0
    w.write(0, 4); // numProgram = 0
    w.write(0, 3); // numLayer = 0

    // --- AudioSpecificConfig, bit-packed (NOT byte aligned here) ---
    if cfg.object_type >= 31 {
        w.write(31, 5);
        w.write((cfg.object_type - 32) as u32, 6);
    } else {
        w.write(cfg.object_type as u32, 5);
    }
    match crate::sf_index_for_rate(cfg.sample_rate) {
        Some(i) => w.write(i as u32, 4),
        None => {
            w.write(0x0F, 4);
            w.write(cfg.sample_rate, 24);
        }
    }
    w.write(cfg.channels as u32, 4);
    // GASpecificConfig: frameLengthFlag, dependsOnCoreCoder, extensionFlag.
    w.write(0, 3);

    w.write(0, 3); // frameLengthType = 0
    w.write(0xFF, 8); // latmBufferFullness = 255 (VBR / "don't care")
    w.write_bool(false); // otherDataPresent
    w.write_bool(false); // crcCheckPresent

    // --- PayloadLengthInfo: 255-escaped byte count ---
    let mut remaining = au.len();
    while remaining >= 255 {
        w.write(255, 8);
        remaining -= 255;
    }
    w.write(remaining as u32, 8);

    // --- PayloadMux ---
    for &b in au {
        w.write(b as u32, 8);
    }

    let body = w.into_bytes();

    // --- audioSyncStream wrapper ---
    let mut out = Vec::with_capacity(LOAS_HEADER_LEN + body.len());
    let hdr = (LOAS_SYNC << 13) | (body.len() as u32 & 0x1FFF);
    out.push((hdr >> 16) as u8);
    out.push((hdr >> 8) as u8);
    out.push(hdr as u8);
    out.extend_from_slice(&body);
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn cfg() -> AudioSpecificConfig {
        AudioSpecificConfig {
            object_type: 2,
            sample_rate: 44100,
            channels: 2,
        }
    }

    /// Writer -> parser round-trip, the basic contract.
    #[test]
    fn loas_round_trips() {
        let au: Vec<u8> = (0..200u32).map(|i| (i * 7 + 3) as u8).collect();
        let frame = write_loas_frame(&cfg(), &au);
        assert!(is_loas(&frame), "writer must emit a valid syncword");
        let got = parse_loas_frame(&frame).expect("parse");
        assert_eq!(got.au, au, "payload must survive");
        assert_eq!(got.config.sample_rate, 44100);
        assert_eq!(got.config.channels, 2);
        assert_eq!(got.config.object_type, 2);
        assert_eq!(got.frame_length, frame.len());
    }

    /// The 255-escape in PayloadLengthInfo must handle lengths either side of the
    /// boundary — an off-by-one there truncates or over-reads every large frame.
    #[test]
    fn payload_length_escape_is_exact() {
        for len in [0usize, 1, 254, 255, 256, 509, 510, 511, 1000] {
            let au: Vec<u8> = (0..len).map(|i| (i % 251) as u8).collect();
            let frame = write_loas_frame(&cfg(), &au);
            let got = parse_loas_frame(&frame).unwrap_or_else(|e| panic!("len {len}: {e}"));
            assert_eq!(got.au.len(), len, "length {len} mis-coded");
            assert_eq!(got.au, au, "length {len} payload corrupted");
        }
    }

    /// A real stream sets `useSameStreamMux` after the first frame; a stateless
    /// parser fails on those, a `LatmReader` must not.
    #[test]
    fn reader_carries_config_across_frames() {
        let au = vec![0xAAu8; 40];
        let first = write_loas_frame(&cfg(), &au);

        // Hand-build a second frame with useSameStreamMux = 1.
        let mut w = BitWriter::new();
        w.write_bool(true); // useSameStreamMux
        w.write(au.len() as u32, 8); // PayloadLengthInfo (< 255)
        for &b in &au {
            w.write(b as u32, 8);
        }
        let body = w.into_bytes();
        let hdr = (LOAS_SYNC << 13) | (body.len() as u32 & 0x1FFF);
        let mut second = vec![(hdr >> 16) as u8, (hdr >> 8) as u8, hdr as u8];
        second.extend_from_slice(&body);

        let mut rd = LatmReader::new();
        let a = rd.parse(&first).expect("first frame");
        assert_eq!(a.au, au);
        let b = rd
            .parse(&second)
            .expect("second frame must reuse the config");
        assert_eq!(b.au, au);
        assert_eq!(b.config.sample_rate, 44100);

        // A fresh reader must REFUSE the config-less frame rather than guess.
        assert!(LatmReader::new().parse(&second).is_err());
    }

    /// Sync detection and resynchronisation.
    #[test]
    fn finds_sync_after_garbage() {
        let au = vec![0x5Au8; 32];
        let frame = write_loas_frame(&cfg(), &au);
        let mut stream = vec![0x00, 0xFF, 0x12, 0x34];
        let offset = stream.len();
        stream.extend_from_slice(&frame);
        assert_eq!(find_sync(&stream), Some(offset));
        let got = parse_loas_frame(&stream[offset..]).unwrap();
        assert_eq!(got.au, au);
    }

    /// A truncated frame must ask for more data, not mis-parse.
    #[test]
    fn truncated_frame_returns_again() {
        let au = vec![0x11u8; 300];
        let frame = write_loas_frame(&cfg(), &au);
        let cut = &frame[..frame.len() - 10];
        assert!(matches!(parse_loas_frame(cut), Err(Error::Again)));
    }

    /// Unsupported mux shapes are refused with a clear error, never mis-decoded.
    #[test]
    fn unsupported_mux_shapes_are_refused() {
        // numLayer != 0
        let mut w = BitWriter::new();
        w.write_bool(false); // useSameStreamMux
        w.write_bool(false); // audioMuxVersion
        w.write_bool(true); // allStreamsSameTimeFraming
        w.write(0, 6); // numSubFrames
        w.write(0, 4); // numProgram
        w.write(1, 3); // numLayer = 1  <-- unsupported
        let body = w.into_bytes();
        let hdr = (LOAS_SYNC << 13) | (body.len() as u32 & 0x1FFF);
        let mut frame = vec![(hdr >> 16) as u8, (hdr >> 8) as u8, hdr as u8];
        frame.extend_from_slice(&body);
        assert!(parse_loas_frame(&frame).is_err());
    }

    /// **End to end**: real AAC through LOAS and back out to PCM. The unit tests
    /// above prove the framing; this proves the framing carries a decodable
    /// stream, which is the only claim that matters for MPEG-TS carriage.
    #[test]
    fn real_aac_survives_loas_carriage() {
        use crate::{AacEncoder, AacEncoderConfig};
        let sr = 44100u32;
        let n = 10 * 1024;
        let pcm: Vec<f32> = (0..n * 2)
            .map(|i| {
                let t = (i / 2) as f32 / sr as f32;
                0.3 * (2.0 * std::f32::consts::PI * 440.0 * t).sin()
            })
            .collect();

        let mut enc = AacEncoder::new(AacEncoderConfig::default());
        enc.push_pcm(&pcm, 2, sr).unwrap();
        enc.finish();

        let cfg = AudioSpecificConfig {
            object_type: 2,
            sample_rate: sr,
            channels: 2,
        };

        // Encode -> LOAS
        let mut stream = Vec::new();
        let mut n_frames = 0usize;
        while let Ok(p) = enc.next_packet() {
            stream.extend_from_slice(&write_loas_frame(&cfg, &p.data));
            n_frames += 1;
        }
        assert!(n_frames > 5, "need several frames to exercise the reader");

        // LOAS -> decode
        let mut rd = LatmReader::new();
        // The transport supplies the config out of band, exactly as an MPEG-TS
        // demuxer would hand it to the decoder.
        let mut dec = crate::AacDecoder::with_config(cfg);
        let mut pos = 0usize;
        let mut decoded = 0usize;
        let mut parsed = 0usize;
        while pos + LOAS_HEADER_LEN < stream.len() {
            let f = rd.parse(&stream[pos..]).expect("LOAS frame");
            assert_eq!(f.config, cfg, "config must survive the transport");
            let audio = dec.decode(&f.au, None).expect("AU must decode");
            decoded += audio.frames();
            pos += f.frame_length;
            parsed += 1;
        }
        assert_eq!(parsed, n_frames, "every frame must be recovered");
        assert!(decoded >= n, "decoded {decoded} of {n} samples");
    }
}
