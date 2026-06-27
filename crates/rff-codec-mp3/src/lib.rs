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

use rff_codec::{Codec, CodecRegistry, Decoder, Encoder};
use rff_core::{CodecId, Error, Frame, MediaType, Packet, Result};

mod bitio;
mod decode;
mod encode;
mod frame;
mod header;
mod tables;

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
    eof: bool,
}

impl Decoder for Mp3Decoder {
    fn send_packet(&mut self, _packet: &Packet) -> Result<()> {
        // brick: frame-sync the packet, parse each header, feed side-info +
        // main-data to `self.state.decode_frame`, queue the resulting AudioFrames.
        Err(Error::Unimplemented("mp3 decode: pipeline not yet built"))
    }

    fn receive_frame(&mut self) -> Result<Frame> {
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
