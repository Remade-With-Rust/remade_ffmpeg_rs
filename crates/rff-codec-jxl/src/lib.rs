//! JPEG XL image **decoder**, backed by the pure-Rust [`jxl_oxide`]
//! (MIT/Apache-2.0, no FFI).
//!
//! JPEG XL is self-describing, so a packet is the whole `.jxl` stream. We render
//! the first keyframe and convert its `f32` channels to packed
//! [`Rgb24`](PixelFormat::Rgb24)/[`Rgba`](PixelFormat::Rgba). Decode only — there
//! is no permissive pure-Rust JPEG XL *encoder* (registered with `encoder: None`).

use std::io::Cursor;

use jxl_oxide::JxlImage;
use rff_codec::{Codec, CodecRegistry, Decoder};
use rff_core::{Error, Frame, MediaType, Packet, PixelFormat, Result, VideoFrame};

/// Register the JPEG XL codec (decode only) into a [`CodecRegistry`].
pub fn register(registry: &mut CodecRegistry) {
    registry.register(Codec {
        id: rff_core::CodecId::Jxl,
        name: "jpegxl",
        long_name: "JPEG XL image",
        media_type: MediaType::Video,
        decoder: Some(|| Box::new(JxlDecoder::default())),
        encoder: None,
    });
}

#[derive(Default)]
struct JxlDecoder {
    frame: Option<Frame>,
    eof: bool,
}

impl Decoder for JxlDecoder {
    fn send_packet(&mut self, packet: &Packet) -> Result<()> {
        let image = JxlImage::builder()
            .read(Cursor::new(&packet.data))
            .map_err(|e| Error::invalid(format!("jxl decode: {e}")))?;
        let render = image
            .render_frame(0)
            .map_err(|e| Error::invalid(format!("jxl decode: {e}")))?;
        let fb = render.image_all_channels();
        let (w, h, channels) = (fb.width(), fb.height(), fb.channels());
        let buf = fb.buf();
        let to_u8 = |v: f32| (v.clamp(0.0, 1.0) * 255.0).round() as u8;

        let (format, planes, stride) = match channels {
            3 => (
                PixelFormat::Rgb24,
                buf.iter().map(|&v| to_u8(v)).collect(),
                w * 3,
            ),
            4 => (
                PixelFormat::Rgba,
                buf.iter().map(|&v| to_u8(v)).collect(),
                w * 4,
            ),
            1 => {
                let mut rgb = Vec::with_capacity(w * h * 3);
                for &v in buf {
                    let g = to_u8(v);
                    rgb.extend_from_slice(&[g, g, g]);
                }
                (PixelFormat::Rgb24, rgb, w * 3)
            }
            2 => {
                let mut rgba = Vec::with_capacity(w * h * 4);
                for ga in buf.chunks_exact(2) {
                    let g = to_u8(ga[0]);
                    rgba.extend_from_slice(&[g, g, g, to_u8(ga[1])]);
                }
                (PixelFormat::Rgba, rgba, w * 4)
            }
            other => {
                return Err(Error::unsupported(format!(
                    "jxl decode: {other}-channel image"
                )))
            }
        };

        self.frame = Some(Frame::Video(VideoFrame {
            width: w as u32,
            height: h as u32,
            format,
            planes: vec![planes],
            strides: vec![stride],
            pts: None,
        }));
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn decode_rejects_garbage() {
        let mut dec = JxlDecoder::default();
        assert!(dec
            .send_packet(&Packet::from_data(0, vec![0u8; 32]))
            .is_err());
    }
}
