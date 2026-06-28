//! AVIF still-image codec, backed by AV1.
//!
//! AVIF is an AV1 *intra* (key) frame wrapped in HEIF/ISOBMFF boxes. We model
//! it as a single-frame video codec: encode accepts one [`Frame`] and emits the
//! AV1 bitstream; decode (when wired) yields one [`Frame`]. The HEIF box
//! wrapping itself is handled at the format layer when an `avif` container
//! lands; this crate is the pixel codec.
//!
//! * **Encode** — over [`rav1e`], the BSD-2-Clause native-Rust AV1 encoder, in
//!   `still_picture` mode (8- and 10-bit planar YUV).
//! * **Decode** — over [`rav1d`], a pure-Rust AV1 decoder, through its safe
//!   Rust API (no `unsafe`, no C); see [`AvifDecoder`].

use std::collections::VecDeque;

use rav1d::{Decoder as Rav1dDec, PixelLayout, PlanarImageComponent, Rav1dError};
use rav1e::prelude::{ChromaSampling, Config, Context, EncoderConfig, EncoderStatus, FrameType};
use rff_codec::{Codec, CodecRegistry, Decoder, Encoder};
use rff_core::{Dictionary, Error, Frame, MediaType, Packet, PixelFormat, Result, VideoFrame};

/// Map an `ffmpeg`-style `-preset` (named or numeric) to a rav1e speed (0 =
/// slowest/best … 10 = fastest). Unknown/absent → 6 (rav1e's balanced default).
fn preset_to_speed(preset: Option<&str>) -> u8 {
    match preset {
        Some(p) => match p.to_ascii_lowercase().as_str() {
            "ultrafast" => 10,
            "superfast" => 9,
            "veryfast" => 8,
            "faster" => 7,
            "fast" => 6,
            "medium" => 5,
            "slow" => 3,
            "slower" => 2,
            "veryslow" | "placebo" => 0,
            n => n.parse::<u8>().unwrap_or(6).min(10),
        },
        None => 6,
    }
}

/// Parse a bitrate like `2M` / `500k` / `128000` into bits per second.
fn parse_bitrate(b: Option<&str>) -> Option<i32> {
    let b = b?.trim();
    let (num, mul) = match b.chars().last() {
        Some('k') | Some('K') => (&b[..b.len() - 1], 1_000),
        Some('m') | Some('M') => (&b[..b.len() - 1], 1_000_000),
        _ => (b, 1),
    };
    num.trim()
        .parse::<f64>()
        .ok()
        .map(|v| (v * mul as f64) as i32)
}

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

/// rav1e is generic over the pixel sample type: 8-bit content uses `u8`,
/// 10/12-bit uses `u16`. We pick the matching context from the first frame and
/// keep it for the life of the encoder.
enum Av1Context {
    Bd8(Context<u8>),
    Bd16(Context<u16>),
}

/// AVIF encoder: bridges the engine's send/receive [`Encoder`] contract onto a
/// [`rav1e`] `Context`.
///
/// The rav1e context is created lazily from the first frame (it needs the
/// dimensions, chroma layout and bit depth up front). Encoded packets are
/// buffered in `queue` and handed out one per `receive_packet`, matching the
/// FFmpeg-style drain loop the rest of the engine expects.
struct AvifEncoder {
    ctx: Option<Av1Context>,
    /// Geometry locked in from the first frame; later frames must match.
    geometry: Option<(u32, u32, PixelFormat)>,
    /// Encoded packets ready to hand out.
    queue: VecDeque<Packet>,
    /// Set once the encoder has been flushed and fully drained.
    eof: bool,
    /// Output rate-control / tuning options (`-crf`, `-preset`, `-b`, ...).
    options: Dictionary,
}

impl AvifEncoder {
    fn new() -> AvifEncoder {
        AvifEncoder {
            ctx: None,
            geometry: None,
            queue: VecDeque::new(),
            eof: false,
            options: Dictionary::new(),
        }
    }

    /// Build the rav1e context for the first frame's geometry + bit depth.
    fn init(&mut self, vf: &VideoFrame) -> Result<()> {
        let chroma = match vf.format {
            PixelFormat::Yuv420p | PixelFormat::Yuv420p10 => ChromaSampling::Cs420,
            PixelFormat::Yuv422p | PixelFormat::Yuv422p10 => ChromaSampling::Cs422,
            PixelFormat::Yuv444p | PixelFormat::Yuv444p10 => ChromaSampling::Cs444,
            other => {
                return Err(Error::unsupported(format!(
                    "avif encode: pixel format `{}` (needs planar YUV)",
                    other.name()
                )))
            }
        };
        let bit_depth = vf.format.bit_depth() as usize;

        let mut enc = EncoderConfig::with_speed_preset(preset_to_speed(self.options.get("preset")));
        enc.width = vf.width as usize;
        enc.height = vf.height as usize;
        enc.bit_depth = bit_depth;
        enc.chroma_sampling = chroma;
        // AVIF is a single key frame; this tunes rav1e for one-shot intra.
        enc.still_picture = true;
        // Rate control: -qp sets rav1e's quantizer directly; -crf (ffmpeg's
        // 0–63 scale) maps onto it ×4; -b targets a bitrate (bits/sec).
        if let Some(qp) = self.options.get_int("qp") {
            enc.quantizer = qp.clamp(0, 255) as usize;
        } else if let Some(crf) = self.options.get_int("crf") {
            enc.quantizer = (crf * 4).clamp(0, 255) as usize;
        }
        if let Some(bitrate) = parse_bitrate(self.options.get("b")) {
            enc.bitrate = bitrate;
        }

        let cfg = Config::new().with_encoder_config(enc);
        let on_err = |e| Error::InvalidData(format!("rav1e config rejected: {e}"));
        let ctx = if bit_depth > 8 {
            Av1Context::Bd16(cfg.new_context::<u16>().map_err(on_err)?)
        } else {
            Av1Context::Bd8(cfg.new_context::<u8>().map_err(on_err)?)
        };

        self.ctx = Some(ctx);
        self.geometry = Some((vf.width, vf.height, vf.format));
        Ok(())
    }

    /// Pull every packet rav1e currently has buffered into `queue`.
    fn pump(&mut self) -> Result<()> {
        loop {
            // Both pixel widths yield the same `(bytes, frame_type)` shape.
            let received = match self.ctx.as_mut() {
                Some(Av1Context::Bd8(c)) => c.receive_packet().map(|p| (p.data, p.frame_type)),
                Some(Av1Context::Bd16(c)) => c.receive_packet().map(|p| (p.data, p.frame_type)),
                None => return Ok(()),
            };
            match received {
                Ok((data, frame_type)) => {
                    let mut out = Packet::from_data(0, data);
                    out.flags.keyframe = frame_type == FrameType::KEY;
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
    fn configure(&mut self, options: &Dictionary) -> Result<()> {
        self.options = options.clone();
        Ok(())
    }

    fn send_frame(&mut self, frame: &Frame) -> Result<()> {
        let vf = match frame {
            Frame::Video(v) => v,
            Frame::Audio(_) => {
                return Err(Error::unsupported(
                    "avif encode: audio frame on a video codec",
                ))
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

        if vf.planes.len() < 3 || vf.strides.len() < 3 {
            return Err(Error::invalid(format!(
                "avif encode: expected 3 planes, got {}",
                vf.planes.len()
            )));
        }

        // 1 byte/sample for 8-bit, 2 for 10-bit (little-endian u16). rav1e's
        // `copy_from_raw_u8` interprets the source by this bytewidth.
        let bytes = vf.format.bytes_per_sample();
        let send_result = match self.ctx.as_mut().expect("context initialized above") {
            Av1Context::Bd8(c) => {
                let mut rframe = c.new_frame();
                for (i, plane) in rframe.planes.iter_mut().enumerate() {
                    plane.copy_from_raw_u8(&vf.planes[i], vf.strides[i], bytes);
                }
                c.send_frame(rframe)
            }
            Av1Context::Bd16(c) => {
                let mut rframe = c.new_frame();
                for (i, plane) in rframe.planes.iter_mut().enumerate() {
                    plane.copy_from_raw_u8(&vf.planes[i], vf.strides[i], bytes);
                }
                c.send_frame(rframe)
            }
        };

        match send_result {
            Ok(()) => {}
            // Buffer is full; draining below will make room.
            Err(EncoderStatus::EnoughData) => {}
            Err(e) => return Err(Error::InvalidData(format!("rav1e send_frame: {e:?}"))),
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
        match self.ctx.as_mut() {
            Some(Av1Context::Bd8(c)) => c.flush(),
            Some(Av1Context::Bd16(c)) => c.flush(),
            // Nothing was ever sent; there is nothing to flush.
            None => {
                self.eof = true;
                return;
            }
        }
        // Drain whatever the flush released; ignore errors here — they'll
        // resurface from receive_packet if the queue ends up empty.
        let _ = self.pump();
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

        match self
            .dec
            .as_mut()
            .unwrap()
            .send_data(buf, None, pts, duration)
        {
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
    // Plane bytes are copied verbatim (1 byte/sample at 8-bit, 2 at 10-bit), so
    // bit depth only affects which pixel format we tag the frame with.
    let format = match (pic.pixel_layout(), pic.bit_depth()) {
        (PixelLayout::I420, 8) => PixelFormat::Yuv420p,
        (PixelLayout::I422, 8) => PixelFormat::Yuv422p,
        (PixelLayout::I444, 8) => PixelFormat::Yuv444p,
        (PixelLayout::I420, 10) => PixelFormat::Yuv420p10,
        (PixelLayout::I422, 10) => PixelFormat::Yuv422p10,
        (PixelLayout::I444, 10) => PixelFormat::Yuv444p10,
        (PixelLayout::I400, _) => {
            return Err(Error::unsupported(
                "avif decode: monochrome (I400) not yet mapped",
            ))
        }
        (_, depth) => {
            return Err(Error::unsupported(format!(
                "avif decode: {depth}-bit depth (only 8- and 10-bit are mapped so far)"
            )))
        }
    };

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
        let Frame::Video(src) = &original else {
            unreachable!()
        };
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

    /// Build a 64×64 YUV420p **10-bit** frame: a horizontal luma gradient over
    /// flat mid-gray chroma. Samples are little-endian `u16`.
    fn gradient_frame_10bit() -> Frame {
        let (w, h) = (64usize, 64usize);
        let mut y = vec![0u8; w * h * 2];
        for row in 0..h {
            for col in 0..w {
                let val = (col * 1023 / (w - 1)) as u16;
                let i = (row * w + col) * 2;
                y[i..i + 2].copy_from_slice(&val.to_le_bytes());
            }
        }
        // Mid-gray chroma at 10-bit is 512.
        let mut chroma = vec![0u8; (w / 2) * (h / 2) * 2];
        for s in chroma.chunks_mut(2) {
            s.copy_from_slice(&512u16.to_le_bytes());
        }
        Frame::Video(VideoFrame {
            width: w as u32,
            height: h as u32,
            format: PixelFormat::Yuv420p10,
            planes: vec![y, chroma.clone(), chroma],
            strides: vec![w * 2, (w / 2) * 2, (w / 2) * 2],
            pts: Some(0),
        })
    }

    #[test]
    fn encode_decode_roundtrip_10bit() {
        let original = gradient_frame_10bit();
        let decoded = decode(encode(&original));

        assert_eq!(decoded.width, 64);
        assert_eq!(decoded.height, 64);
        assert_eq!(decoded.format, PixelFormat::Yuv420p10);

        // Compare luma as little-endian u16 samples, with a 10-bit-scaled
        // tolerance (~4× the 8-bit bound).
        let Frame::Video(src) = &original else {
            unreachable!()
        };
        let (w, h) = (src.width as usize, src.height as usize);
        let mut total_diff = 0u64;
        for row in 0..h {
            let src_row = &src.planes[0][row * src.strides[0]..][..w * 2];
            let dec_row = &decoded.planes[0][row * decoded.strides[0]..][..w * 2];
            for (a, b) in src_row.chunks_exact(2).zip(dec_row.chunks_exact(2)) {
                let av = u16::from_le_bytes([a[0], a[1]]) as i32;
                let bv = u16::from_le_bytes([b[0], b[1]]) as i32;
                total_diff += (av - bv).unsigned_abs() as u64;
            }
        }
        let mean_abs_diff = total_diff as f64 / (w * h) as f64;
        assert!(
            mean_abs_diff < 120.0,
            "10-bit luma drifted too far: mean abs diff {mean_abs_diff:.2}"
        );
    }
}
