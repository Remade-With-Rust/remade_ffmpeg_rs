//! PNG still-image codec, backed by the pure-Rust [`png`] crate.
//!
//! PNG is self-describing (its `IHDR` carries size + color type), so decode
//! needs no stream parameters: a packet *is* the whole PNG file. We decode to
//! packed [`Rgb24`](PixelFormat::Rgb24)/[`Rgba`](PixelFormat::Rgba) frames and
//! encode those back. To bridge PNG (RGB) and the YUV codecs (AVIF), insert a
//! `-vf format=yuv420p` / `format=rgb24` conversion.

use std::io::Cursor;

use png::{BitDepth, ColorType, Transformations};
use rff_codec::{Codec, CodecRegistry, Decoder, Encoder};
use rff_core::{Error, Frame, MediaType, Packet, PixelFormat, Result, VideoFrame};

/// Register the PNG codec into a [`CodecRegistry`].
pub fn register(registry: &mut CodecRegistry) {
    registry.register(Codec {
        id: rff_core::CodecId::Png,
        name: "png",
        long_name: "PNG (Portable Network Graphics) image",
        media_type: MediaType::Video,
        decoder: Some(|| Box::new(PngDecoder::default())),
        encoder: Some(|| Box::new(PngEncoder::default())),
    });
}

// ---------------------------------------------------------------------------
// Decoder
// ---------------------------------------------------------------------------

#[derive(Default)]
struct PngDecoder {
    frame: Option<Frame>,
    eof: bool,
}

impl Decoder for PngDecoder {
    fn send_packet(&mut self, packet: &Packet) -> Result<()> {
        self.frame = Some(decode_png(&packet.data)?);
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

fn decode_png(data: &[u8]) -> Result<Frame> {
    let mut decoder = png::Decoder::new(Cursor::new(data));
    // Normalize: expand palette / sub-8-bit, and reduce 16-bit to 8-bit.
    decoder.set_transformations(Transformations::EXPAND | Transformations::STRIP_16);
    let mut reader = decoder
        .read_info()
        .map_err(|e| Error::invalid(format!("png decode: {e}")))?;

    let mut buf = vec![0u8; reader.output_buffer_size()];
    let info = reader
        .next_frame(&mut buf)
        .map_err(|e| Error::invalid(format!("png decode: {e}")))?;
    buf.truncate(info.buffer_size());
    let (w, h) = (info.width as usize, info.height as usize);

    // Normalize whatever the PNG decoded to into packed RGB or RGBA.
    let (format, planes, stride) = match reader.output_color_type().0 {
        ColorType::Rgb => (PixelFormat::Rgb24, buf, w * 3),
        ColorType::Rgba => (PixelFormat::Rgba, buf, w * 4),
        ColorType::Grayscale => {
            let mut rgb = vec![0u8; w * h * 3];
            for (i, &g) in buf.iter().enumerate() {
                rgb[i * 3..i * 3 + 3].copy_from_slice(&[g, g, g]);
            }
            (PixelFormat::Rgb24, rgb, w * 3)
        }
        ColorType::GrayscaleAlpha => {
            let mut rgba = vec![0u8; w * h * 4];
            for (i, ga) in buf.chunks_exact(2).enumerate() {
                rgba[i * 4..i * 4 + 4].copy_from_slice(&[ga[0], ga[0], ga[0], ga[1]]);
            }
            (PixelFormat::Rgba, rgba, w * 4)
        }
        other => {
            return Err(Error::unsupported(format!(
                "png decode: unexpected color type {other:?}"
            )))
        }
    };

    Ok(Frame::Video(VideoFrame {
        width: info.width,
        height: info.height,
        format,
        planes: vec![planes],
        strides: vec![stride],
        pts: None,
    }))
}

// ---------------------------------------------------------------------------
// Encoder
// ---------------------------------------------------------------------------

#[derive(Default)]
struct PngEncoder {
    packet: Option<Packet>,
    eof: bool,
}

impl Encoder for PngEncoder {
    fn send_frame(&mut self, frame: &Frame) -> Result<()> {
        let vf = match frame {
            Frame::Video(v) => v,
            Frame::Audio(_) => {
                return Err(Error::unsupported("png encode: audio frame on an image codec"))
            }
        };
        self.packet = Some(Packet::from_data(0, encode_png(vf)?));
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

fn encode_png(vf: &VideoFrame) -> Result<Vec<u8>> {
    let (color, channels) = match vf.format {
        PixelFormat::Rgb24 => (ColorType::Rgb, 3usize),
        PixelFormat::Rgba => (ColorType::Rgba, 4usize),
        other => {
            return Err(Error::unsupported(format!(
                "png encode: needs rgb24/rgba, got `{}` (convert with -vf format=rgb24)",
                other.name()
            )))
        }
    };
    let (w, h) = (vf.width as usize, vf.height as usize);
    let row = w * channels;
    let stride = vf.strides[0];

    // png wants tightly packed rows; repack if the source stride has padding.
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
    {
        let mut encoder = png::Encoder::new(&mut out, vf.width, vf.height);
        encoder.set_color(color);
        encoder.set_depth(BitDepth::Eight);
        let mut writer = encoder
            .write_header()
            .map_err(|e| Error::invalid(format!("png encode: {e}")))?;
        writer
            .write_image_data(&packed)
            .map_err(|e| Error::invalid(format!("png encode: {e}")))?;
    }
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn rgb_frame(w: u32, h: u32) -> Frame {
        let (wi, hi) = (w as usize, h as usize);
        let mut rgb = vec![0u8; wi * hi * 3];
        for j in 0..hi {
            for i in 0..wi {
                let o = (j * wi + i) * 3;
                rgb[o] = (i * 255 / (wi - 1)) as u8;
                rgb[o + 1] = (j * 255 / (hi - 1)) as u8;
                rgb[o + 2] = 128;
            }
        }
        Frame::Video(VideoFrame {
            width: w,
            height: h,
            format: PixelFormat::Rgb24,
            planes: vec![rgb],
            strides: vec![wi * 3],
            pts: None,
        })
    }

    #[test]
    fn png_encode_decode_is_lossless() {
        let original = rgb_frame(40, 24);
        let Frame::Video(src) = &original else { unreachable!() };

        let bytes = encode_png(src).unwrap();
        assert_eq!(&bytes[1..4], b"PNG"); // PNG signature

        let Frame::Video(decoded) = decode_png(&bytes).unwrap() else {
            unreachable!()
        };
        assert_eq!((decoded.width, decoded.height), (40, 24));
        assert_eq!(decoded.format, PixelFormat::Rgb24);
        // PNG is lossless: pixels must match exactly.
        assert_eq!(decoded.planes[0], src.planes[0]);
    }
}
