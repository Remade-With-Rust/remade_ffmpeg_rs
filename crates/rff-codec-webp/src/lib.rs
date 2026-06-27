//! WebP still-image codec, backed by the pure-Rust [`image_webp`].
//!
//! Decode handles both VP8 (lossy) and VP8L (lossless), yielding packed
//! [`Rgb24`](PixelFormat::Rgb24) or [`Rgba`](PixelFormat::Rgba). Encode is
//! **lossless** (image-webp's encoder). Bridge to the YUV codecs with
//! `-vf format=...`.

use std::io::Cursor;

use image_webp::{ColorType, WebPDecoder, WebPEncoder};
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
    frame: Option<Frame>,
    eof: bool,
}

impl Decoder for WebpDecoder {
    fn send_packet(&mut self, packet: &Packet) -> Result<()> {
        self.frame = Some(decode_webp(&packet.data)?);
        Ok(())
    }

    fn receive_frame(&mut self) -> Result<Frame> {
        if let Some(frame) = self.frame.take() {
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

fn decode_webp(data: &[u8]) -> Result<Frame> {
    let mut decoder = WebPDecoder::new(Cursor::new(data))
        .map_err(|e| Error::invalid(format!("webp decode: {e}")))?;
    let (w, h) = decoder.dimensions();
    let has_alpha = decoder.has_alpha();
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
    Ok(Frame::Video(VideoFrame {
        width: w,
        height: h,
        format,
        planes: vec![buf],
        strides: vec![stride],
        pts: None,
    }))
}

#[derive(Default)]
struct WebpEncoder {
    packet: Option<Packet>,
    eof: bool,
}

impl Encoder for WebpEncoder {
    fn send_frame(&mut self, frame: &Frame) -> Result<()> {
        let vf = match frame {
            Frame::Video(v) => v,
            Frame::Audio(_) => {
                return Err(Error::unsupported("webp encode: audio frame on an image codec"))
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
    WebPEncoder::new(&mut out)
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

        let Frame::Video(decoded) = decode_webp(&bytes).unwrap() else {
            unreachable!()
        };
        assert_eq!((decoded.width, decoded.height), (w, h));
        // Lossless: an RGB source round-trips exactly (decoded may carry alpha).
        let got_rgb: Vec<u8> = match decoded.format {
            PixelFormat::Rgb24 => decoded.planes[0].clone(),
            PixelFormat::Rgba => decoded.planes[0].chunks_exact(4).flat_map(|p| p[0..3].to_vec()).collect(),
            other => panic!("unexpected format {other:?}"),
        };
        assert_eq!(got_rgb, rgb);
    }
}
