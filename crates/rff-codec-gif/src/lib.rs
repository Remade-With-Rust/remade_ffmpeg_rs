//! GIF still-image codec, backed by the pure-Rust [`gif`] crate.
//!
//! GIF is animated and palette-based; we model it as a still image — decode
//! yields the **first** frame as packed [`Rgba`](PixelFormat::Rgba), and encode
//! writes a single quantized frame (≤256 colors, so encode is lossy). Bridge to
//! the YUV codecs with `-vf format=...`.

use std::io::Cursor;

use gif::{ColorOutput, DecodeOptions, Encoder, Frame as GifFrame};
use rff_codec::{Codec, CodecRegistry, Decoder, Encoder as RffEncoder};
use rff_core::{Error, Frame, MediaType, Packet, PixelFormat, Result, VideoFrame};

/// Register the GIF codec into a [`CodecRegistry`].
pub fn register(registry: &mut CodecRegistry) {
    registry.register(Codec {
        id: rff_core::CodecId::Gif,
        name: "gif",
        long_name: "GIF (Graphics Interchange Format) image",
        media_type: MediaType::Video,
        decoder: Some(|| Box::new(GifDecoder::default())),
        encoder: Some(|| Box::new(GifEncoder::default())),
    });
}

#[derive(Default)]
struct GifDecoder {
    frame: Option<Frame>,
    eof: bool,
}

impl Decoder for GifDecoder {
    fn send_packet(&mut self, packet: &Packet) -> Result<()> {
        self.frame = Some(decode_gif(&packet.data)?);
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

fn decode_gif(data: &[u8]) -> Result<Frame> {
    let mut options = DecodeOptions::new();
    options.set_color_output(ColorOutput::RGBA);
    let mut decoder = options
        .read_info(Cursor::new(data))
        .map_err(|e| Error::invalid(format!("gif decode: {e}")))?;
    let frame = decoder
        .read_next_frame()
        .map_err(|e| Error::invalid(format!("gif decode: {e}")))?
        .ok_or_else(|| Error::invalid("gif decode: no frames"))?;

    let (w, h) = (frame.width as u32, frame.height as u32);
    Ok(Frame::Video(VideoFrame {
        width: w,
        height: h,
        format: PixelFormat::Rgba,
        planes: vec![frame.buffer.to_vec()],
        strides: vec![w as usize * 4],
        pts: None,
    }))
}

#[derive(Default)]
struct GifEncoder {
    packet: Option<Packet>,
    eof: bool,
}

impl RffEncoder for GifEncoder {
    fn send_frame(&mut self, frame: &Frame) -> Result<()> {
        let vf = match frame {
            Frame::Video(v) => v,
            Frame::Audio(_) => {
                return Err(Error::unsupported(
                    "gif encode: audio frame on an image codec",
                ))
            }
        };
        self.packet = Some(Packet::from_data(0, encode_gif(vf)?));
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

fn encode_gif(vf: &VideoFrame) -> Result<Vec<u8>> {
    if vf.width > u16::MAX as u32 || vf.height > u16::MAX as u32 {
        return Err(Error::unsupported("gif encode: dimensions exceed 65535"));
    }
    let (w, h) = (vf.width as u16, vf.height as u16);

    // Build a tightly packed copy, then quantize to a palette frame.
    let gif_frame = match vf.format {
        PixelFormat::Rgb24 => GifFrame::from_rgb(w, h, &packed(vf, 3)),
        PixelFormat::Rgba => GifFrame::from_rgba_speed(w, h, &mut packed(vf, 4), 10),
        other => {
            return Err(Error::unsupported(format!(
                "gif encode: needs rgb24/rgba, got `{}` (convert with -vf format=rgb24)",
                other.name()
            )))
        }
    };

    let mut out = Vec::new();
    {
        let mut encoder = Encoder::new(&mut out, w, h, &[])
            .map_err(|e| Error::invalid(format!("gif encode: {e}")))?;
        encoder
            .write_frame(&gif_frame)
            .map_err(|e| Error::invalid(format!("gif encode: {e}")))?;
    }
    Ok(out)
}

/// Tightly pack plane 0 (`channels` bytes/pixel), dropping any stride padding.
fn packed(vf: &VideoFrame, channels: usize) -> Vec<u8> {
    let (w, h) = (vf.width as usize, vf.height as usize);
    let row = w * channels;
    let stride = vf.strides[0];
    if stride == row {
        return vf.planes[0].clone();
    }
    let mut p = Vec::with_capacity(row * h);
    for j in 0..h {
        p.extend_from_slice(&vf.planes[0][j * stride..j * stride + row]);
    }
    p
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn gif_encode_decode_roundtrips() {
        // Few distinct colors so palette quantization is lossless here.
        let (w, h) = (8u32, 8u32);
        let mut rgb = vec![0u8; (w * h * 3) as usize];
        for (i, px) in rgb.chunks_exact_mut(3).enumerate() {
            let v = ((i % 4) * 80) as u8;
            px.copy_from_slice(&[v, v, v]);
        }
        let src = VideoFrame {
            width: w,
            height: h,
            format: PixelFormat::Rgb24,
            planes: vec![rgb],
            strides: vec![(w * 3) as usize],
            pts: None,
        };

        let bytes = encode_gif(&src).unwrap();
        assert_eq!(&bytes[0..4], b"GIF8");

        let Frame::Video(decoded) = decode_gif(&bytes).unwrap() else {
            unreachable!()
        };
        assert_eq!((decoded.width, decoded.height), (w, h));
        assert_eq!(decoded.format, PixelFormat::Rgba);
    }
}
