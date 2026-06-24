//! H.264 / AVC video codec.
//!
//! Status: **scaffold**. The codec is registered (so it shows up in
//! `ffmpeg -codecs` and can be selected with `-c:v h264`) and the
//! decode/encode state machines are wired, but the bitstream work is not yet
//! implemented — each stage currently returns [`Error::Unimplemented`].
//!
//! Implementation order to come: NAL unit splitting → SPS/PPS parsing →
//! slice/macroblock decode → motion compensation + deblocking. See
//! `docs/architecture.md` for the roadmap.

use rff_codec::{Codec, CodecRegistry, Decoder, Encoder};
use rff_core::{Error, Frame, MediaType, Packet, Result};

/// Register the H.264 codec into a [`CodecRegistry`].
pub fn register(registry: &mut CodecRegistry) {
    registry.register(Codec {
        id: rff_core::CodecId::H264,
        name: "h264",
        long_name: "H.264 / AVC / MPEG-4 AVC / MPEG-4 part 10",
        media_type: MediaType::Video,
        decoder: Some(|| Box::new(H264Decoder::new())),
        encoder: Some(|| Box::new(H264Encoder::new())),
    });
}

/// H.264 decoder state. (Reference frame buffers, parameter sets, etc. will
/// live here.)
struct H264Decoder {
    _private: (),
}

impl H264Decoder {
    fn new() -> H264Decoder {
        H264Decoder { _private: () }
    }
}

impl Decoder for H264Decoder {
    fn send_packet(&mut self, _packet: &Packet) -> Result<()> {
        Err(Error::Unimplemented("h264 decode: send_packet"))
    }

    fn receive_frame(&mut self) -> Result<Frame> {
        Err(Error::Unimplemented("h264 decode: receive_frame"))
    }
}

/// H.264 encoder state.
struct H264Encoder {
    _private: (),
}

impl H264Encoder {
    fn new() -> H264Encoder {
        H264Encoder { _private: () }
    }
}

impl Encoder for H264Encoder {
    fn send_frame(&mut self, _frame: &Frame) -> Result<()> {
        Err(Error::Unimplemented("h264 encode: send_frame"))
    }

    fn receive_packet(&mut self) -> Result<Packet> {
        Err(Error::Unimplemented("h264 encode: receive_packet"))
    }
}
