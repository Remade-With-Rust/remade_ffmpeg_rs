//! `rff-codec` — the codec abstraction layer (FFmpeg's `libavcodec` core,
//! minus the codecs themselves).
//!
//! It defines:
//! * the [`Decoder`] and [`Encoder`] traits, using the same *send / receive*
//!   shape as modern FFmpeg (`avcodec_send_packet` / `avcodec_receive_frame`),
//! * a [`Codec`] descriptor that advertises a codec's identity and which
//!   directions (decode/encode) it supports,
//! * a [`CodecRegistry`] that individual codec crates register into.
//!
//! Concrete codecs live in their own crates (`rff-codec-h264`, ...) and are
//! pulled together by the `rff` facade.

use std::collections::HashMap;

use rff_core::{
    CodecId, Dictionary, Error, Frame, MediaType, Packet, PixelFormat, Result, SampleFormat,
};

/// Stream parameters handed to a decoder before the first packet. Self-describing
/// codecs (AV1, PNG, ...) can ignore these; raw/parametric codecs (PCM, and
/// codecs that carry config out of band like H.264 SPS/PPS) need them.
#[derive(Debug, Clone, Default)]
pub struct CodecParams {
    pub codec_id: CodecId,
    // Video
    pub width: u32,
    pub height: u32,
    pub pixel_format: Option<PixelFormat>,
    // Audio
    pub sample_rate: u32,
    pub channels: u16,
    pub sample_format: Option<SampleFormat>,
    /// Codec-private init data (SPS/PPS, OpusHead, ...).
    pub extradata: Vec<u8>,
}

/// Decodes compressed [`Packet`]s into raw [`Frame`]s.
///
/// Drive it the FFmpeg way: feed packets with [`send_packet`](Decoder::send_packet),
/// then pull frames with [`receive_frame`](Decoder::receive_frame) until it
/// returns [`Error::Again`] (needs more input) or [`Error::Eof`].
pub trait Decoder: Send {
    /// Receive the stream's parameters before decoding begins. Default: ignore
    /// them (the bitstream is self-describing). Called once, after construction.
    fn configure(&mut self, _params: &CodecParams) -> Result<()> {
        Ok(())
    }

    /// Submit one compressed packet for decoding.
    fn send_packet(&mut self, packet: &Packet) -> Result<()>;

    /// Retrieve the next decoded frame, if one is ready.
    ///
    /// Returns [`Error::Again`] when more packets are needed, or [`Error::Eof`]
    /// once fully drained.
    fn receive_frame(&mut self) -> Result<Frame>;

    /// Signal end-of-input so buffered frames can be flushed out via
    /// subsequent `receive_frame` calls. Default: no buffering.
    fn flush(&mut self) {}
}

/// Encodes raw [`Frame`]s into compressed [`Packet`]s. Mirror of [`Decoder`].
pub trait Encoder: Send {
    /// Receive output options before encoding begins — rate control and tuning:
    /// `crf`/`qp` (quality), `preset` (speed/quality trade-off), `b` (bitrate),
    /// `pass` (1 or 2). Default: ignore them. Called once, after construction.
    fn configure(&mut self, _options: &Dictionary) -> Result<()> {
        Ok(())
    }

    /// The input sample rates this encoder accepts, or `None` for "any rate".
    /// The transcode pipeline resamples audio to the nearest accepted rate
    /// before feeding frames (mirrors FFmpeg's automatic `aresample`).
    fn accepted_sample_rates(&self) -> Option<Vec<u32>> {
        None
    }

    /// Submit one raw frame for encoding.
    fn send_frame(&mut self, frame: &Frame) -> Result<()>;

    /// Retrieve the next encoded packet, if one is ready.
    fn receive_packet(&mut self) -> Result<Packet>;

    /// Signal end-of-input to drain buffered packets.
    fn flush(&mut self) {}
}

/// Factory function producing a fresh decoder instance.
pub type DecoderFactory = fn() -> Box<dyn Decoder>;
/// Factory function producing a fresh encoder instance.
pub type EncoderFactory = fn() -> Box<dyn Encoder>;

/// Static description of a codec and the implementations available for it.
pub struct Codec {
    pub id: CodecId,
    /// Short name as used on the command line (`h264`, `opus`, ...).
    pub name: &'static str,
    /// Human-readable description (shown by `ffmpeg -codecs`).
    pub long_name: &'static str,
    pub media_type: MediaType,
    /// Present if this codec can decode.
    pub decoder: Option<DecoderFactory>,
    /// Present if this codec can encode.
    pub encoder: Option<EncoderFactory>,
}

impl Codec {
    pub fn can_decode(&self) -> bool {
        self.decoder.is_some()
    }
    pub fn can_encode(&self) -> bool {
        self.encoder.is_some()
    }
}

/// Holds every codec known to a running engine. Codec crates call
/// [`register`](CodecRegistry::register) to add themselves.
#[derive(Default)]
pub struct CodecRegistry {
    by_id: HashMap<CodecId, Codec>,
    by_name: HashMap<&'static str, CodecId>,
}

impl CodecRegistry {
    pub fn new() -> CodecRegistry {
        CodecRegistry::default()
    }

    /// Register a codec. A later registration for the same [`CodecId`] replaces
    /// the earlier one (lets downstreams override a built-in).
    pub fn register(&mut self, codec: Codec) {
        self.by_name.insert(codec.name, codec.id);
        self.by_id.insert(codec.id, codec);
    }

    /// Instantiate a decoder for `id`, or [`Error::DecoderNotFound`].
    pub fn find_decoder(&self, id: CodecId) -> Result<Box<dyn Decoder>> {
        self.by_id
            .get(&id)
            .and_then(|c| c.decoder)
            .map(|factory| factory())
            .ok_or(Error::DecoderNotFound(id))
    }

    /// Instantiate an encoder for `id`, or [`Error::EncoderNotFound`].
    pub fn find_encoder(&self, id: CodecId) -> Result<Box<dyn Encoder>> {
        self.by_id
            .get(&id)
            .and_then(|c| c.encoder)
            .map(|factory| factory())
            .ok_or(Error::EncoderNotFound(id))
    }

    /// Look up a codec descriptor by its canonical name.
    pub fn by_name(&self, name: &str) -> Option<&Codec> {
        self.by_name.get(name).and_then(|id| self.by_id.get(id))
    }

    /// Look up a codec descriptor by id.
    pub fn by_id(&self, id: CodecId) -> Option<&Codec> {
        self.by_id.get(&id)
    }

    /// Iterate all registered codecs (order unspecified).
    pub fn iter(&self) -> impl Iterator<Item = &Codec> {
        self.by_id.values()
    }

    pub fn len(&self) -> usize {
        self.by_id.len()
    }

    pub fn is_empty(&self) -> bool {
        self.by_id.is_empty()
    }
}
