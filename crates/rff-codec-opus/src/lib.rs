//! Opus audio codec.
//!
//! Status: **scaffold**. Registered and wired, but the codec body
//! (range decoder → SILK / CELT layers) is not yet implemented; each stage
//! returns [`Error::Unimplemented`].

use rff_codec::{Codec, CodecRegistry, Decoder, Encoder};
use rff_core::{Error, Frame, MediaType, Packet, Result};

/// Register the Opus codec into a [`CodecRegistry`].
pub fn register(registry: &mut CodecRegistry) {
    registry.register(Codec {
        id: rff_core::CodecId::Opus,
        name: "opus",
        long_name: "Opus (Opus Interactive Audio Codec)",
        media_type: MediaType::Audio,
        decoder: Some(|| Box::new(OpusDecoder::new())),
        encoder: Some(|| Box::new(OpusEncoder::new())),
    });
}

struct OpusDecoder {
    _private: (),
}

impl OpusDecoder {
    fn new() -> OpusDecoder {
        OpusDecoder { _private: () }
    }
}

impl Decoder for OpusDecoder {
    fn send_packet(&mut self, _packet: &Packet) -> Result<()> {
        Err(Error::Unimplemented("opus decode: send_packet"))
    }

    fn receive_frame(&mut self) -> Result<Frame> {
        Err(Error::Unimplemented("opus decode: receive_frame"))
    }
}

struct OpusEncoder {
    _private: (),
}

impl OpusEncoder {
    fn new() -> OpusEncoder {
        OpusEncoder { _private: () }
    }
}

impl Encoder for OpusEncoder {
    fn send_frame(&mut self, _frame: &Frame) -> Result<()> {
        Err(Error::Unimplemented("opus encode: send_frame"))
    }

    fn receive_packet(&mut self) -> Result<Packet> {
        Err(Error::Unimplemented("opus encode: receive_packet"))
    }
}
