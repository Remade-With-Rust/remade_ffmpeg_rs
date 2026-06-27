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

use header::FrameHeader;
pub use decode::Mp3Decode;
pub use encode::Mp3Encode;

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
            let hb = [self.buf[pos], self.buf[pos + 1], self.buf[pos + 2], self.buf[pos + 3]];
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

#[derive(Default)]
struct Mp3Encoder {
    state: Mp3Encode,
    eof: bool,
}

impl Encoder for Mp3Encoder {
    fn send_frame(&mut self, _frame: &Frame) -> Result<()> {
        // brick: gather a frame's worth of PCM, call `self.state.encode_frame`,
        // queue the resulting MP3 frame as a Packet.
        Err(Error::Unimplemented("mp3 encode: pipeline not yet built"))
    }

    fn receive_packet(&mut self) -> Result<Packet> {
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
        while let Ok(Frame::Audio(af)) = dec.receive_frame() {
            assert_eq!(af.sample_rate, 44100);
            assert_eq!(af.channels, 1);
            assert_eq!(af.planes[0].len(), af.samples * af.channels as usize * 4);
            frames += 1;
            samples += af.samples;
        }
        eprintln!("[MP3] decoded frames={frames} samples={samples}");
        assert!(frames > 0, "must decode at least one frame from real data");
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
        for (v, m, len) in [(V1, Stereo, 32), (V1, Mono, 17), (V2, Stereo, 17), (V2, Mono, 9)] {
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
