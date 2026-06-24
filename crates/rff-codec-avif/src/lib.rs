//! AVIF still-image codec, backed by AV1.
//!
//! AVIF is an AV1 *intra* (key) frame wrapped in HEIF/ISOBMFF boxes. We model
//! it as a single-frame video codec: encode accepts one [`Frame`] and emits the
//! AV1 bitstream; decode (when wired) yields one [`Frame`]. The HEIF box
//! wrapping itself is handled at the format layer when an `avif` container
//! lands; this crate is the pixel codec.
//!
//! * **Encode** — implemented over [`rav1e`], the BSD-2-Clause native-Rust AV1
//!   encoder, in `still_picture` mode.
//! * **Decode** — not yet wired. AV1 decode needs an AV1 decoder dependency,
//!   and the obvious candidates each trade off against a project rule (license
//!   vs. Rust-API vs. no-C); see [`AvifDecoder`].

use std::collections::VecDeque;

use rav1d::{Decoder as Rav1dDec, PixelLayout, PlanarImageComponent, Rav1dError};
use rav1e::prelude::{
    ChromaSampling, Config, Context, EncoderConfig, EncoderStatus, FrameType,
};
use rff_codec::{Codec, CodecRegistry, Decoder, Encoder};
use rff_core::{Error, Frame, MediaType, Packet, PixelFormat, Result, VideoFrame};

/// Register the AVIF codec into a [`CodecRegistry`].
pub fn register(registry: &mut CodecRegistry) {
    registry.register(Codec {
        id: rff_core::CodecId::Avif,
        name: "avif",
        long_name: "AVIF (AV1 Image File Format)",
        media_type: MediaType::Video,
        decoder: Some(|| Box::new(AvifDecoder::new())),
        encoder: Some(|| Box::new(AvifEncoder::new())),
    });
}

// ---------------------------------------------------------------------------
// Encoder (rav1e)
// ---------------------------------------------------------------------------

/// AVIF encoder: bridges the engine's send/receive [`Encoder`] contract onto a
/// [`rav1e`] `Context`.
///
/// The rav1e context is created lazily from the first frame (it needs the
/// dimensions and chroma layout up front). Encoded packets are buffered in
/// `queue` and handed out one per `receive_packet`, matching the FFmpeg-style
/// drain loop the rest of the engine expects.
struct AvifEncoder {
    ctx: Option<Context<u8>>,
    /// Geometry locked in from the first frame; later frames must match.
    geometry: Option<(u32, u32, PixelFormat)>,
    /// Encoded packets ready to hand out.
    queue: VecDeque<Packet>,
    /// Set once the encoder has been flushed and fully drained.
    eof: bool,
}

impl AvifEncoder {
    fn new() -> AvifEncoder {
        AvifEncoder {
            ctx: None,
            geometry: None,
            queue: VecDeque::new(),
            eof: false,
        }
    }

    /// Build the rav1e context for the first frame's geometry.
    fn init(&mut self, vf: &VideoFrame) -> Result<()> {
        let chroma = match vf.format {
            PixelFormat::Yuv420p => ChromaSampling::Cs420,
            PixelFormat::Yuv422p => ChromaSampling::Cs422,
            PixelFormat::Yuv444p => ChromaSampling::Cs444,
            other => {
                return Err(Error::unsupported(format!(
                    "avif encode: pixel format `{}` (needs planar YUV)",
                    other.name()
                )))
            }
        };

        let mut enc = EncoderConfig::with_speed_preset(6);
        enc.width = vf.width as usize;
        enc.height = vf.height as usize;
        enc.bit_depth = 8;
        enc.chroma_sampling = chroma;
        // AVIF is a single key frame; this tunes rav1e for one-shot intra.
        enc.still_picture = true;

        let cfg = Config::new().with_encoder_config(enc);
        let ctx = cfg
            .new_context::<u8>()
            .map_err(|e| Error::InvalidData(format!("rav1e config rejected: {e}")))?;

        self.ctx = Some(ctx);
        self.geometry = Some((vf.width, vf.height, vf.format));
        Ok(())
    }

    /// Pull every packet rav1e currently has buffered into `queue`.
    fn pump(&mut self) -> Result<()> {
        let ctx = self
            .ctx
            .as_mut()
            .expect("pump called before context init");
        loop {
            match ctx.receive_packet() {
                Ok(pkt) => {
                    let mut out = Packet::from_data(0, pkt.data);
                    out.flags.keyframe = pkt.frame_type == FrameType::KEY;
                    self.queue.push_back(out);
                }
                // A frame was consumed but produced no output packet yet.
                Err(EncoderStatus::Encoded) => continue,
                // Fully drained after a flush.
                Err(EncoderStatus::LimitReached) => {
                    self.eof = true;
                    break;
                }
                Err(EncoderStatus::Failure) => {
                    return Err(Error::InvalidData("rav1e encode failure".into()))
                }
                // NeedMoreData / EnoughData / NotReady — nothing more right now.
                Err(_) => break,
            }
        }
        Ok(())
    }
}

impl Encoder for AvifEncoder {
    fn send_frame(&mut self, frame: &Frame) -> Result<()> {
        let vf = match frame {
            Frame::Video(v) => v,
            Frame::Audio(_) => {
                return Err(Error::unsupported("avif encode: audio frame on a video codec"))
            }
        };

        match self.geometry {
            None => self.init(vf)?,
            Some(geom) if geom != (vf.width, vf.height, vf.format) => {
                return Err(Error::unsupported(
                    "avif encode: frame geometry changed mid-stream (still-image codec)",
                ))
            }
            Some(_) => {}
        }

        let expected_planes = match vf.format {
            PixelFormat::Yuv420p | PixelFormat::Yuv422p | PixelFormat::Yuv444p => 3,
            // init() already rejected anything else.
            _ => unreachable!(),
        };
        if vf.planes.len() < expected_planes || vf.strides.len() < expected_planes {
            return Err(Error::invalid(format!(
                "avif encode: expected {expected_planes} planes, got {}",
                vf.planes.len()
            )));
        }

        let ctx = self.ctx.as_mut().expect("context initialized above");
        let mut rframe = ctx.new_frame();
        for (i, plane) in rframe.planes.iter_mut().enumerate() {
            // 1 byte per sample (8-bit); source stride may exceed plane width.
            plane.copy_from_raw_u8(&vf.planes[i], vf.strides[i], 1);
        }

        match ctx.send_frame(rframe) {
            Ok(()) => {}
            // Buffer is full; draining below will make room.
            Err(EncoderStatus::EnoughData) => {}
            Err(e) => {
                return Err(Error::InvalidData(format!("rav1e send_frame: {e:?}")))
            }
        }
        self.pump()
    }

    fn receive_packet(&mut self) -> Result<Packet> {
        if let Some(pkt) = self.queue.pop_front() {
            return Ok(pkt);
        }
        if self.eof {
            Err(Error::Eof)
        } else {
            Err(Error::Again)
        }
    }

    fn flush(&mut self) {
        if let Some(ctx) = self.ctx.as_mut() {
            ctx.flush();
            // Drain whatever the flush released; ignore errors here — they'll
            // resurface from receive_packet if the queue ends up empty.
            let _ = self.pump();
        } else {
            // Nothing was ever sent; there is nothing to flush.
            self.eof = true;
        }
    }
}

// ---------------------------------------------------------------------------
// Decoder (rav1d)
// ---------------------------------------------------------------------------

/// AVIF decoder: bridges the engine's send/receive [`Decoder`] contract onto a
/// [`rav1d`] decoder via its safe Rust API (no `unsafe`, no C).
///
/// The rav1d decoder is created lazily on the first packet (construction is
/// fallible, but the engine's decoder factory is not). Decoded pictures are
/// copied into `queue` and handed out one per `receive_frame`.
struct AvifDecoder {
    dec: Option<Rav1dDec>,
    /// Decoded frames ready to hand out.
    queue: VecDeque<Frame>,
    /// Set once the caller has flushed; receive then drains to `Eof`.
    eof: bool,
}

impl AvifDecoder {
    fn new() -> AvifDecoder {
        AvifDecoder {
            dec: None,
            queue: VecDeque::new(),
            eof: false,
        }
    }

    /// Pull every picture rav1d currently has ready into `queue`.
    fn drain_pictures(&mut self) -> Result<()> {
        let dec = self.dec.as_mut().expect("decoder initialized");
        loop {
            match dec.get_picture() {
                Ok(pic) => self.queue.push_back(picture_to_frame(&pic)?),
                // No more pictures available right now.
                Err(Rav1dError::TryAgain) => break,
                Err(e) => return Err(map_rav1d_err(e)),
            }
        }
        Ok(())
    }
}

impl Decoder for AvifDecoder {
    fn send_packet(&mut self, packet: &Packet) -> Result<()> {
        if self.dec.is_none() {
            self.dec = Some(Rav1dDec::new().map_err(map_rav1d_err)?);
        }

        let buf = packet.data.clone().into_boxed_slice();
        let pts = packet.pts;
        let duration = Some(packet.duration);

        match self.dec.as_mut().unwrap().send_data(buf, None, pts, duration) {
            Ok(()) => {}
            // The decoder couldn't take all the data yet: pull pictures out to
            // make room, then push the remaining pending bytes through.
            Err(Rav1dError::TryAgain) => {
                self.drain_pictures()?;
                loop {
                    match self.dec.as_mut().unwrap().send_pending_data() {
                        Ok(()) => break,
                        Err(Rav1dError::TryAgain) => self.drain_pictures()?,
                        Err(e) => return Err(map_rav1d_err(e)),
                    }
                }
            }
            Err(e) => return Err(map_rav1d_err(e)),
        }

        self.drain_pictures()
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
        // Pictures are pulled eagerly on send, so by here the queue already
        // holds everything. Note: rav1d's own `flush()` is for seeking (it
        // *discards* buffered frames), so we deliberately do not call it.
        if self.dec.is_some() {
            let _ = self.drain_pictures();
        }
        self.eof = true;
    }
}

/// Convert a decoded rav1d [`Picture`](rav1d::Picture) into the engine's
/// [`Frame`]. Copies plane data out (the picture is reference-counted and freed
/// when dropped).
fn picture_to_frame(pic: &rav1d::Picture) -> Result<Frame> {
    let format = match pic.pixel_layout() {
        PixelLayout::I420 => PixelFormat::Yuv420p,
        PixelLayout::I422 => PixelFormat::Yuv422p,
        PixelLayout::I444 => PixelFormat::Yuv444p,
        PixelLayout::I400 => {
            return Err(Error::unsupported("avif decode: monochrome (I400) not yet mapped"))
        }
    };

    if pic.bit_depth() != 8 {
        return Err(Error::unsupported(format!(
            "avif decode: {}-bit depth (only 8-bit is mapped so far)",
            pic.bit_depth()
        )));
    }

    let mut planes = Vec::with_capacity(3);
    let mut strides = Vec::with_capacity(3);
    for component in [
        PlanarImageComponent::Y,
        PlanarImageComponent::U,
        PlanarImageComponent::V,
    ] {
        planes.push(pic.plane(component).to_vec());
        strides.push(pic.stride(component) as usize);
    }

    Ok(Frame::Video(VideoFrame {
        width: pic.width(),
        height: pic.height(),
        format,
        planes,
        strides,
        pts: pic.timestamp(),
    }))
}

/// Map a rav1d error onto the engine's error type, preserving the
/// "needs more input" control-flow signal.
fn map_rav1d_err(e: Rav1dError) -> Error {
    match e {
        Rav1dError::TryAgain => Error::Again,
        other => Error::InvalidData(format!("rav1d decode: {other:?}")),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use rff_core::frame::VideoFrame;

    /// Build a 64×64 YUV420p frame: a horizontal luma gradient over flat
    /// mid-gray chroma. Tightly packed (stride == row width).
    fn gradient_frame() -> Frame {
        let (w, h) = (64usize, 64usize);
        let mut y = vec![0u8; w * h];
        for row in 0..h {
            for col in 0..w {
                y[row * w + col] = (col * 255 / (w - 1)) as u8;
            }
        }
        let chroma = vec![128u8; (w / 2) * (h / 2)];
        Frame::Video(VideoFrame {
            width: w as u32,
            height: h as u32,
            format: PixelFormat::Yuv420p,
            planes: vec![y, chroma.clone(), chroma],
            strides: vec![w, w / 2, w / 2],
            pts: Some(0),
        })
    }

    /// Encode one frame to an AV1 bitstream (concatenating any OBU packets).
    fn encode(frame: &Frame) -> Vec<u8> {
        let mut enc = AvifEncoder::new();
        enc.send_frame(frame).expect("send_frame");
        enc.flush();
        let mut bytes = Vec::new();
        loop {
            match enc.receive_packet() {
                Ok(p) => bytes.extend_from_slice(&p.data),
                Err(Error::Eof) => break,
                Err(Error::Again) => break, // nothing more buffered
                Err(e) => panic!("encode receive_packet: {e}"),
            }
        }
        assert!(!bytes.is_empty(), "encoder produced no bitstream");
        bytes
    }

    /// Decode an AV1 bitstream back to a single frame.
    fn decode(bitstream: Vec<u8>) -> VideoFrame {
        let mut dec = AvifDecoder::new();
        let mut pkt = Packet::from_data(0, bitstream);
        pkt.flags.keyframe = true;
        dec.send_packet(&pkt).expect("send_packet");
        dec.flush();
        match dec.receive_frame() {
            Ok(Frame::Video(v)) => v,
            Ok(Frame::Audio(_)) => panic!("decoded audio from a video codec"),
            Err(e) => panic!("decode receive_frame: {e}"),
        }
    }

    #[test]
    fn encode_decode_roundtrip() {
        let original = gradient_frame();
        let decoded = decode(encode(&original));

        // Geometry survives the round-trip exactly.
        assert_eq!(decoded.width, 64);
        assert_eq!(decoded.height, 64);
        assert_eq!(decoded.format, PixelFormat::Yuv420p);

        // AV1 is lossy, so compare the luma plane with tolerance. The encoded
        // gradient should come back close to the original.
        let Frame::Video(src) = &original else { unreachable!() };
        let (w, h) = (src.width as usize, src.height as usize);
        let mut total_diff = 0u64;
        for row in 0..h {
            let src_row = &src.planes[0][row * src.strides[0]..][..w];
            let dec_row = &decoded.planes[0][row * decoded.strides[0]..][..w];
            for (a, b) in src_row.iter().zip(dec_row) {
                total_diff += (*a as i16 - *b as i16).unsigned_abs() as u64;
            }
        }
        let mean_abs_diff = total_diff as f64 / (w * h) as f64;
        assert!(
            mean_abs_diff < 30.0,
            "luma drifted too far in round-trip: mean abs diff {mean_abs_diff:.2}"
        );
    }
}
