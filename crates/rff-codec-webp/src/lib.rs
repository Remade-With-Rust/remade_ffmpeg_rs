//! WebP still-image codec, backed by the pure-Rust [`rusty_webp`].
//!
//! Decode handles both VP8 (lossy) and VP8L (lossless), yielding packed
//! [`Rgb24`](PixelFormat::Rgb24) or [`Rgba`](PixelFormat::Rgba). Encode is
//! **lossless** (image-webp's encoder). Bridge to the YUV codecs with
//! `-vf format=...`.

use std::io::Cursor;

use rusty_webp::{ColorType, WebPDecoder, WebPEncoder};
use rff_codec::{Codec, CodecRegistry, Decoder, Encoder};
use rff_core::{Error, Frame, MediaType, Packet, PixelFormat, Result, VideoFrame};

/// Register the WebP codec into a [`CodecRegistry`].
pub fn register(registry: &mut CodecRegistry) {
    registry.register(Codec {
        id: rff_core::CodecId::Webp,
        name: "webp",
        long_name: "WebP image (VP8 / VP8L)",
        media_type: MediaType::Video,
        decoder: Some(|| Box::new(WebpDecoder::default())),
        encoder: Some(|| Box::new(WebpEncoder::default())),
    });
}

#[derive(Default)]
struct WebpDecoder {
    frames: std::collections::VecDeque<Frame>,
    eof: bool,
}

impl Decoder for WebpDecoder {
    fn send_packet(&mut self, packet: &Packet) -> Result<()> {
        self.frames = decode_webp(&packet.data)?;
        Ok(())
    }

    fn receive_frame(&mut self) -> Result<Frame> {
        if let Some(frame) = self.frames.pop_front() {
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

fn decode_webp(data: &[u8]) -> Result<std::collections::VecDeque<Frame>> {
    // Chroma upsampling for lossy (VP8) files. Bilinear ("fancy") matches
    // libwebp's dwebp default and is ours too; `RFF_WEBP_UPSAMPLE=simple`
    // selects the cheaper nearest-value method (dwebp -nofancy) for A/Bs.
    let mut options = rusty_webp::WebPDecodeOptions::default();
    if std::env::var("RFF_WEBP_UPSAMPLE").as_deref() == Ok("simple") {
        options.lossy_upsampling = rusty_webp::UpsamplingMethod::Simple;
    }
    let mut decoder = WebPDecoder::new_with_options(Cursor::new(data), options)
        .map_err(|e| Error::invalid(format!("webp decode: {e}")))?;
    let (w, h) = decoder.dimensions();
    let has_alpha = decoder.has_alpha();

    // Animated WebP: decode every composited canvas frame, with pts in
    // milliseconds accumulated from the per-frame durations (the demuxer sets
    // a 1/1000 time base for animated streams). FFmpeg's native webp decoder
    // errors out on these files entirely.
    if decoder.is_animated() {
        let mut frames = std::collections::VecDeque::new();
        let mut pts_ms = 0i64;
        for _ in 0..decoder.num_frames() {
            let mut buf = vec![0u8; w as usize * h as usize * 4];
            let duration = decoder
                .read_frame(&mut buf)
                .map_err(|e| Error::invalid(format!("webp decode (animation): {e}")))?;
            frames.push_back(Frame::Video(VideoFrame {
                width: w,
                height: h,
                format: PixelFormat::Rgba,
                planes: vec![buf],
                strides: vec![w as usize * 4],
                pts: Some(pts_ms),
            }));
            pts_ms += i64::from(duration);
        }
        return Ok(frames);
    }

    // Still lossy (VP8) images without alpha decode straight to their native
    // YUV 4:2:0 planes (BT.601 limited-range — the demuxer labels the stream)
    // with no chroma upsampling and no RGB round-trip, exactly like FFmpeg's
    // webp decoder. Setting `RFF_WEBP_UPSAMPLE` opts back into RGB output with
    // the named upsampling method (libwebp-dwebp look).
    if std::env::var("RFF_WEBP_UPSAMPLE").is_err() {
        if let Some(yuv) = decoder
            .read_yuv420()
            .map_err(|e| Error::invalid(format!("webp decode: {e}")))?
        {
            return Ok(std::collections::VecDeque::from([Frame::Video(VideoFrame {
                width: w,
                height: h,
                format: PixelFormat::Yuv420p,
                strides: vec![yuv.y_stride, yuv.uv_stride, yuv.uv_stride],
                planes: vec![yuv.y, yuv.u, yuv.v],
                pts: None,
            })]));
        }
    }

    let channels = if has_alpha { 4 } else { 3 };

    let mut buf = vec![0u8; w as usize * h as usize * channels];
    decoder
        .read_image(&mut buf)
        .map_err(|e| Error::invalid(format!("webp decode: {e}")))?;

    let (format, stride) = if has_alpha {
        (PixelFormat::Rgba, w as usize * 4)
    } else {
        (PixelFormat::Rgb24, w as usize * 3)
    };
    Ok(std::collections::VecDeque::from([Frame::Video(VideoFrame {
        width: w,
        height: h,
        format,
        planes: vec![buf],
        strides: vec![stride],
        pts: None,
    })]))
}

#[derive(Default)]
struct WebpEncoder {
    packet: Option<Packet>,
    eof: bool,
}

impl Encoder for WebpEncoder {
    /// Packed RGB only â€” this encoder has no Y'CbCr path, so the pipeline
    /// converts planar input (e.g. straight from the JPEG decoder) for us.
    fn accepted_pixel_formats(&self) -> Option<Vec<PixelFormat>> {
        Some(vec![PixelFormat::Rgb24, PixelFormat::Rgba])
    }

    fn send_frame(&mut self, frame: &Frame) -> Result<()> {
        let vf = match frame {
            Frame::Video(v) => v,
            Frame::Audio(_) => {
                return Err(Error::unsupported(
                    "webp encode: audio frame on an image codec",
                ))
            }
        };
        self.packet = Some(Packet::from_data(0, encode_webp(vf)?));
        Ok(())
    }

    fn receive_packet(&mut self) -> Result<Packet> {
        if let Some(packet) = self.packet.take() {
            return Ok(packet);
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

fn encode_webp(vf: &VideoFrame) -> Result<Vec<u8>> {
    let (color, channels) = match vf.format {
        PixelFormat::Rgb24 => (ColorType::Rgb8, 3usize),
        PixelFormat::Rgba => (ColorType::Rgba8, 4usize),
        other => {
            return Err(Error::unsupported(format!(
                "webp encode: needs rgb24/rgba, got `{}` (convert with -vf format=rgb24)",
                other.name()
            )))
        }
    };
    let (w, h) = (vf.width as usize, vf.height as usize);
    let row = w * channels;
    let stride = vf.strides[0];
    let packed: Vec<u8> = if stride == row {
        vf.planes[0].clone()
    } else {
        let mut p = Vec::with_capacity(row * h);
        for j in 0..h {
            p.extend_from_slice(&vf.planes[0][j * stride..j * stride + row]);
        }
        p
    };

    let mut out = Vec::new();
    let mut encoder = WebPEncoder::new(&mut out);
    // `RFF_WEBP_BACKREFS=0` selects the runs-only fast path (no LZ77 search,
    // no color cache): bigger files, fastest possible encode. Also the A/B
    // arm for pricing the compression machinery.
    if std::env::var("RFF_WEBP_BACKREFS").as_deref() == Ok("0") {
        let mut params = rusty_webp::EncoderParams::default();
        params.use_backrefs = false;
        encoder.set_params(params);
    }
    encoder
        .encode(&packed, vf.width, vf.height, color)
        .map_err(|e| Error::invalid(format!("webp encode: {e}")))?;
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn webp_lossless_encode_decode_roundtrips() {
        let (w, h) = (32u32, 24u32);
        let (wi, hi) = (w as usize, h as usize);
        let mut rgb = vec![0u8; wi * hi * 3];
        for j in 0..hi {
            for i in 0..wi {
                let o = (j * wi + i) * 3;
                rgb[o] = (i * 255 / (wi - 1)) as u8;
                rgb[o + 1] = (j * 255 / (hi - 1)) as u8;
                rgb[o + 2] = 64;
            }
        }
        let src = VideoFrame {
            width: w,
            height: h,
            format: PixelFormat::Rgb24,
            planes: vec![rgb.clone()],
            strides: vec![wi * 3],
            pts: None,
        };

        let bytes = encode_webp(&src).unwrap();
        assert_eq!(&bytes[0..4], b"RIFF");
        assert_eq!(&bytes[8..12], b"WEBP");

        let Frame::Video(decoded) = decode_webp(&bytes).unwrap().pop_front().unwrap() else {
            unreachable!()
        };
        assert_eq!((decoded.width, decoded.height), (w, h));
        // Lossless: an RGB source round-trips exactly (decoded may carry alpha).
        let got_rgb: Vec<u8> = match decoded.format {
            PixelFormat::Rgb24 => decoded.planes[0].clone(),
            PixelFormat::Rgba => decoded.planes[0]
                .chunks_exact(4)
                .flat_map(|p| p[0..3].to_vec())
                .collect(),
            other => panic!("unexpected format {other:?}"),
        };
        assert_eq!(got_rgb, rgb);
    }
}
