//! `rff-codec-mp3` — an in-house, pure-Rust MP3 (MPEG-1/2 Audio Layer III)
//! decoder **and** encoder.
//!
//! Why in-house: the robust Rust MP3 crate (Symphonia) is MPL-2.0, which trips
//! our no-copyleft-in-core license gate; the permissive one (puremp3) is
//! incomplete. MP3's patents expired in 2017 and Layer III is exhaustively
//! documented, so we build our own — the same path as AAC and VP9. See
//! `docs/ffmpeg-parity.md`.
//!
//! ## Layout (the framework, built brick by brick)
//!
//! * shared: [`header`] (frame header), [`frame`] (side-info + types),
//!   [`bitio`] (MSB-first bit I/O), [`tables`] (ISO constant tables)
//! * [`decode`]: side-info → reservoir → Huffman → scalefactors → requantize →
//!   stereo → antialias → IMDCT → synthesis filterbank → PCM
//! * [`encode`]: analysis filterbank → MDCT → psychoacoustic model → two-loop
//!   quantizer → Huffman → bitstream
//!
//! The pipeline wiring is in place; each DSP stage is a `todo!()`/`Unimplemented`
//! brick. The public [`Decoder`]/[`Encoder`] return a labelled `Unimplemented`
//! until the bricks are laid, so a transcode resolves the whole graph and stops
//! cleanly at MP3.
#![allow(dead_code)] // Scaffold: stage bricks are wired but not yet implemented.

use std::collections::VecDeque;

use rff_codec::{Codec, CodecRegistry, Decoder, Encoder};
use rff_core::{AudioFrame, CodecId, Error, Frame, MediaType, Packet, Result, SampleFormat};

mod bitio;
mod decode;
mod encode;
mod frame;
mod header;
mod tables;

/// MP3 encoder experiment harness — brick tracking, corpus, metrics, variant
/// sweeps. Opt-in behind the `lab` feature; see docs/mp3-lab.md.
#[cfg(feature = "lab")]
pub mod lab;

pub use decode::Mp3Decode;
pub use encode::Mp3Encode;
use header::FrameHeader;

/// Register the MP3 codec (decoder + encoder) into a [`CodecRegistry`].
pub fn register(registry: &mut CodecRegistry) {
    registry.register(Codec {
        id: CodecId::Mp3,
        name: "mp3",
        long_name: "MP3 (MPEG-1/2 Audio Layer III)",
        media_type: MediaType::Audio,
        decoder: Some(|| Box::new(Mp3Decoder::default())),
        encoder: Some(|| Box::new(Mp3Encoder::default())),
    });
}

#[derive(Default)]
struct Mp3Decoder {
    state: Mp3Decode,
    /// Accumulated bytes awaiting frame-sync (packets may split/join frames).
    buf: Vec<u8>,
    queue: VecDeque<Frame>,
    eof: bool,
}

impl Mp3Decoder {
    /// Frame-sync the buffer: for each complete frame, split header / side-info /
    /// main-data, decode it, and queue an `AudioFrame`. Leaves a trailing partial
    /// frame in `buf` for the next packet.
    fn parse_frames(&mut self) {
        let mut pos = 0;
        while pos + 4 <= self.buf.len() {
            // Sync = 11 set bits: 0xFF then top 3 bits of the next byte.
            if self.buf[pos] != 0xFF || self.buf[pos + 1] & 0xE0 != 0xE0 {
                pos += 1;
                continue;
            }
            let hb = [
                self.buf[pos],
                self.buf[pos + 1],
                self.buf[pos + 2],
                self.buf[pos + 3],
            ];
            let header = match FrameHeader::parse(hb) {
                Ok(h) => h,
                Err(_) => {
                    pos += 1;
                    continue;
                }
            };
            let frame_size = header.frame_size();
            if frame_size < 4 {
                pos += 1;
                continue;
            }
            if pos + frame_size > self.buf.len() {
                break; // incomplete frame — wait for more data
            }

            let crc = if header.crc_protected { 2 } else { 0 };
            let si_start = pos + 4 + crc;
            let si_len = header.side_info_len();
            let main_start = si_start + si_len;
            if main_start > pos + frame_size {
                pos += 1;
                continue;
            }
            let side_info = self.buf[si_start..main_start].to_vec();
            let main_data = self.buf[main_start..pos + frame_size].to_vec();

            if let Ok(pcm) = self.state.decode_frame(&header, &side_info, &main_data) {
                let channels = header.channel_mode.channels().max(1);
                let mut bytes = Vec::with_capacity(pcm.len() * 4);
                for s in &pcm {
                    bytes.extend_from_slice(&s.to_le_bytes());
                }
                self.queue.push_back(Frame::Audio(AudioFrame {
                    sample_rate: header.sample_rate,
                    channels: channels as u16,
                    format: SampleFormat::F32,
                    planes: vec![bytes],
                    samples: pcm.len() / channels,
                    pts: None,
                }));
            }
            pos += frame_size;
        }
        self.buf.drain(0..pos);
    }
}

impl Decoder for Mp3Decoder {
    fn send_packet(&mut self, packet: &Packet) -> Result<()> {
        self.buf.extend_from_slice(&packet.data);
        self.parse_frames();
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

/// Floor-3 MP3 encoder: accumulates **mono** PCM (stereo is downmixed), emits one
/// MP3 frame per 1152 samples. Dumb-but-valid — MPEG-1, 128 kbps CBR, flat
/// scalefactors, no psychoacoustics yet.
#[derive(Default)]
struct Mp3Encoder {
    state: Mp3Encode,
    header: Option<FrameHeader>,
    /// Accumulated mono samples awaiting a full frame.
    pcm: Vec<f32>,
    queue: VecDeque<rff_core::Packet>,
    eof: bool,
}

/// Build the fixed Floor-3 frame header from the input's sample rate.
fn encoder_header(sample_rate: u32) -> Result<FrameHeader> {
    let version = match sample_rate {
        32000 | 44100 | 48000 => header::MpegVersion::V1,
        _ => {
            return Err(Error::unsupported(
                "mp3 encode: sample rate must be 32/44.1/48 kHz (MPEG-1)",
            ))
        }
    };
    Ok(FrameHeader {
        version,
        crc_protected: false,
        bitrate_kbps: 128,
        sample_rate,
        padding: false,
        channel_mode: frame::ChannelMode::Mono,
        copyright: false,
        original: true,
        emphasis: 0,
    })
}

impl Mp3Encoder {
    /// Emit a frame for each full 1152-sample block accumulated.
    fn drain_frames(&mut self) {
        let Some(header) = self.header.clone() else {
            return;
        };
        let spf = header.version.samples_per_frame();
        while self.pcm.len() >= spf {
            let block: Vec<f32> = self.pcm.drain(0..spf).collect();
            if let Ok(bytes) = self.state.encode_frame(&header, &block) {
                self.queue.push_back(Packet::from_data(0, bytes));
            }
        }
    }
}

impl Encoder for Mp3Encoder {
    fn send_frame(&mut self, frame: &Frame) -> Result<()> {
        let Frame::Audio(af) = frame else {
            return Ok(());
        };
        if self.header.is_none() {
            self.header = Some(encoder_header(af.sample_rate)?);
        }
        // Downmix interleaved f32 to mono.
        let data = &af.planes[0];
        let ch = (af.channels as usize).max(1);
        for s in 0..af.samples {
            let mut sum = 0.0f32;
            for c in 0..ch {
                let off = (s * ch + c) * 4;
                if off + 4 <= data.len() {
                    sum += f32::from_le_bytes([
                        data[off],
                        data[off + 1],
                        data[off + 2],
                        data[off + 3],
                    ]);
                }
            }
            self.pcm.push(sum / ch as f32);
        }
        self.drain_frames();
        Ok(())
    }

    fn receive_packet(&mut self) -> Result<Packet> {
        if let Some(p) = self.queue.pop_front() {
            return Ok(p);
        }
        if self.eof {
            Err(Error::Eof)
        } else {
            Err(Error::Again)
        }
    }

    fn flush(&mut self) {
        // Pad the tail to a whole frame and encode it.
        if let Some(header) = self.header.clone() {
            let spf = header.version.samples_per_frame();
            if !self.pcm.is_empty() {
                let padded = self.pcm.len().div_ceil(spf) * spf;
                self.pcm.resize(padded, 0.0);
                self.drain_frames();
            }
        }
        self.eof = true;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Helpers for the encode→decode pipeline gate.
    fn pcm_to_bytes(pcm: &[f32]) -> Vec<u8> {
        let mut b = Vec::with_capacity(pcm.len() * 4);
        for &s in pcm {
            b.extend_from_slice(&s.to_le_bytes());
        }
        b
    }

    fn encode_mono(input: &[f32], sample_rate: u32) -> Vec<u8> {
        let mut enc = Mp3Encoder::default();
        enc.send_frame(&Frame::Audio(AudioFrame {
            sample_rate,
            channels: 1,
            format: SampleFormat::F32,
            planes: vec![pcm_to_bytes(input)],
            samples: input.len(),
            pts: None,
        }))
        .unwrap();
        enc.flush();
        let mut mp3 = Vec::new();
        while let Ok(p) = enc.receive_packet() {
            mp3.extend_from_slice(&p.data);
        }
        mp3
    }

    fn decode_mono(mp3: Vec<u8>) -> Vec<f32> {
        let mut dec = Mp3Decoder::default();
        dec.send_packet(&Packet::from_data(0, mp3)).unwrap();
        dec.flush();
        let mut out = Vec::new();
        while let Ok(Frame::Audio(af)) = dec.receive_frame() {
            for c in af.planes[0].chunks_exact(4) {
                out.push(f32::from_le_bytes([c[0], c[1], c[2], c[3]]));
            }
        }
        out
    }

    /// Best-aligned reconstruction SNR (dB) of `out` vs `reference`, searching the
    /// codec delay and skipping warm-up at both ends.
    fn best_snr(reference: &[f32], out: &[f32]) -> f64 {
        let (mut best, skip) = (f64::NEG_INFINITY, 2304usize);
        for delay in 0..3000 {
            let mut sig = 0f64;
            let mut err = 0f64;
            let mut n = 0;
            let mut i = skip;
            while i + delay < out.len() && i < reference.len() {
                let r = reference[i] as f64;
                sig += r * r;
                err += (r - out[i + delay] as f64).powi(2);
                n += 1;
                i += 1;
            }
            if n > 5000 && err > 0.0 {
                best = best.max(10.0 * (sig / err).log10());
            }
        }
        best
    }

    /// **C4 — the pipeline gate.** A multi-tone signal (which exercises Q6's
    /// non-flat scalefactors) round-trips PCM → encoder → decoder → PCM well above
    /// the noise floor, and the `.mp3` decodes in FFmpeg (checked out-of-band; see
    /// docs/mp3-encoder-plan.md).
    #[test]
    fn encode_decode_pipeline_multitone() {
        let sr = 44100u32;
        let n = 16 * 1152;
        let input: Vec<f32> = (0..n)
            .map(|i| {
                let t = i as f32 / sr as f32;
                let pi2 = 2.0 * std::f32::consts::PI;
                0.3 * (pi2 * 600.0 * t).sin()
                    + 0.2 * (pi2 * 2300.0 * t).sin()
                    + 0.12 * (pi2 * 9000.0 * t).sin()
            })
            .collect();

        let mp3 = encode_mono(&input, sr);
        assert!(!mp3.is_empty(), "encoder produced no data");
        assert_eq!(mp3[0], 0xFF);
        assert_eq!(mp3[1] & 0xE0, 0xE0);
        if let Ok(path) = std::env::var("MP3_ENC_OUT") {
            std::fs::write(path, &mp3).expect("write MP3_ENC_OUT");
        }

        let out = decode_mono(mp3);
        assert!(out.len() > n / 2, "decoder produced too few samples");

        let snr = best_snr(&input, &out);
        eprintln!("[C4] encode→decode multitone SNR {snr:.1} dB");
        assert!(snr > 20.0, "round-trip SNR too low: {snr:.1} dB");
    }

    /// Decode a real MP3 file (path in `MP3_REF`) and report structure. Validates
    /// frame-sync (skips ID3), header/side-info parsing, and main-data extraction
    /// on real data. Output is silent until D[]/codebooks are laid; this checks
    /// the *structure* (frame count, sample count, no panics, finite samples).
    #[test]
    fn decode_real_mp3_structure() {
        let Ok(path) = std::env::var("MP3_REF") else {
            return; // self-skip when not running the reference harness
        };
        let data = std::fs::read(&path).expect("read MP3_REF");
        let mut dec = Mp3Decoder::default();
        dec.send_packet(&Packet::from_data(0, data)).unwrap();
        dec.flush();

        let mut frames = 0usize;
        let mut samples = 0usize;
        let mut pcm: Vec<u8> = Vec::new();
        while let Ok(Frame::Audio(af)) = dec.receive_frame() {
            assert_eq!(af.sample_rate, 44100);
            assert_eq!(af.channels, 1);
            assert_eq!(af.planes[0].len(), af.samples * af.channels as usize * 4);
            pcm.extend_from_slice(&af.planes[0]);
            frames += 1;
            samples += af.samples;
        }
        eprintln!("[MP3] decoded frames={frames} samples={samples}");
        assert!(frames > 0, "must decode at least one frame from real data");
        if let Ok(out) = std::env::var("MP3_OUT") {
            std::fs::write(out, &pcm).expect("write MP3_OUT");
        }
    }

    #[test]
    fn registers_as_audio_codec() {
        let mut reg = CodecRegistry::new();
        register(&mut reg);
        let codec = reg.by_name("mp3").expect("mp3 registered");
        assert_eq!(codec.id, CodecId::Mp3);
        assert_eq!(codec.media_type, MediaType::Audio);
        assert!(codec.can_decode() && codec.can_encode());
    }

    #[test]
    fn header_parse_roundtrip() {
        // MPEG-1 Layer III, 128 kbps, 44.1 kHz, stereo, no CRC, no padding.
        let bytes = [0xFF, 0xFB, 0x90, 0x00];
        let h = header::FrameHeader::parse(bytes).unwrap();
        assert_eq!(h.version, header::MpegVersion::V1);
        assert_eq!(h.bitrate_kbps, 128);
        assert_eq!(h.sample_rate, 44100);
        assert_eq!(h.channel_mode, frame::ChannelMode::Stereo);
        assert!(!h.crc_protected && !h.padding);
        assert_eq!(h.frame_size(), 417);
        assert_eq!(h.to_bytes(), bytes, "header must round-trip bit-exactly");
    }

    #[test]
    fn header_rejects_non_layer3_and_bad_sync() {
        // Layer II (0b10 in the layer field): byte1 = 1111_1101.
        assert!(header::FrameHeader::parse([0xFF, 0xFD, 0x90, 0x00]).is_err());
        // Broken sync.
        assert!(header::FrameHeader::parse([0x00, 0x00, 0x00, 0x00]).is_err());
    }

    fn hdr(version: header::MpegVersion, mode: frame::ChannelMode) -> header::FrameHeader {
        header::FrameHeader {
            version,
            crc_protected: false,
            bitrate_kbps: 128,
            sample_rate: 44100,
            padding: false,
            channel_mode: mode,
            copyright: false,
            original: true,
            emphasis: 0,
        }
    }

    #[test]
    fn sideinfo_bit_accounting_all_layouts() {
        use frame::ChannelMode::{Mono, Stereo};
        use header::MpegVersion::{V1, V2};
        // (version, channel mode, expected side-info length in bytes)
        for (v, m, len) in [
            (V1, Stereo, 32),
            (V1, Mono, 17),
            (V2, Stereo, 17),
            (V2, Mono, 9),
        ] {
            let h = hdr(v, m);
            assert_eq!(h.side_info_len(), len);
            // An all-zero block parses cleanly; parse()'s debug_assert verifies
            // the field widths sum to exactly len*8 bits for this layout.
            let si = decode::sideinfo::parse(&h, &vec![0u8; len]).unwrap();
            assert_eq!(si.main_data_begin, 0);
        }
    }

    #[test]
    fn frame_size_matches_spec_example() {
        // MPEG-1 L3, 128 kbps, 44100 Hz, no padding → 417 bytes.
        let h = header::FrameHeader {
            version: header::MpegVersion::V1,
            crc_protected: false,
            bitrate_kbps: 128,
            sample_rate: 44100,
            padding: false,
            channel_mode: frame::ChannelMode::Stereo,
            copyright: false,
            original: true,
            emphasis: 0,
        };
        assert_eq!(h.frame_size(), 417);
        assert_eq!(h.side_info_len(), 32);
    }
}
