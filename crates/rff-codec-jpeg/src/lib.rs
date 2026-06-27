//! JPEG (a.k.a. MJPEG) still-image codec.
//!
//! Decode is backed by the pure-Rust [`jpeg_decoder`]; encode by the pure-Rust
//! [`jpeg_encoder`]. Like PNG, JPEG is self-describing, so a packet is the whole
//! file. We decode to packed [`Rgb24`](PixelFormat::Rgb24) frames and encode
//! those back; bridge to the YUV codecs with `-vf format=yuv420p` / `rgb24`.

use std::io::Cursor;

use jpeg_encoder::{ColorType, Encoder};
use rff_codec::{Codec, CodecRegistry, Decoder, Encoder as RffEncoder};
use rff_core::{Error, Frame, MediaType, Packet, PixelFormat, Result, VideoFrame};

/// Default encode quality (1–100) when no `-q` option is given.
const DEFAULT_QUALITY: u8 = 90;

/// Register the JPEG codec into a [`CodecRegistry`].
pub fn register(registry: &mut CodecRegistry) {
    registry.register(Codec {
        id: rff_core::CodecId::Jpeg,
        name: "mjpeg",
        long_name: "JPEG / MJPEG (Motion JPEG) image",
        media_type: MediaType::Video,
        decoder: Some(|| Box::new(JpegDecoder::default())),
        encoder: Some(|| Box::new(JpegEncoder::default())),
    });
}

#[derive(Default)]
struct JpegDecoder {
    frame: Option<Frame>,
    eof: bool,
}

impl Decoder for JpegDecoder {
    fn send_packet(&mut self, packet: &Packet) -> Result<()> {
        self.frame = Some(decode_jpeg(&packet.data)?);
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

fn decode_jpeg(data: &[u8]) -> Result<Frame> {
    let mut decoder = jpeg_decoder::Decoder::new(Cursor::new(data));
    let pixels = decoder
        .decode()
        .map_err(|e| Error::invalid(format!("jpeg decode: {e}")))?;
    let info = decoder
        .info()
        .ok_or_else(|| Error::invalid("jpeg decode: missing image info"))?;
    let (w, h) = (info.width as usize, info.height as usize);

    let (planes, stride) = match info.pixel_format {
        jpeg_decoder::PixelFormat::RGB24 => (pixels, w * 3),
        jpeg_decoder::PixelFormat::L8 => {
            // Grayscale → replicate into packed RGB.
            let mut rgb = vec![0u8; w * h * 3];
            for (i, &g) in pixels.iter().enumerate() {
                rgb[i * 3..i * 3 + 3].copy_from_slice(&[g, g, g]);
            }
            (rgb, w * 3)
        }
        other => {
            return Err(Error::unsupported(format!(
                "jpeg decode: pixel format {other:?} (only RGB24/L8)"
            )))
        }
    };

    Ok(Frame::Video(VideoFrame {
        width: info.width as u32,
        height: info.height as u32,
        format: PixelFormat::Rgb24,
        planes: vec![planes],
        strides: vec![stride],
        pts: None,
    }))
}

#[derive(Default)]
struct JpegEncoder {
    packet: Option<Packet>,
    eof: bool,
}

impl RffEncoder for JpegEncoder {
    fn send_frame(&mut self, frame: &Frame) -> Result<()> {
        let vf = match frame {
            Frame::Video(v) => v,
            Frame::Audio(_) => {
                return Err(Error::unsupported("jpeg encode: audio frame on an image codec"))
            }
        };
        self.packet = Some(Packet::from_data(0, encode_jpeg(vf)?));
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

fn encode_jpeg(vf: &VideoFrame) -> Result<Vec<u8>> {
    let (color, channels) = match vf.format {
        PixelFormat::Rgb24 => (ColorType::Rgb, 3usize),
        PixelFormat::Rgba => (ColorType::Rgba, 4usize),
        other => {
            return Err(Error::unsupported(format!(
                "jpeg encode: needs rgb24/rgba, got `{}` (convert with -vf format=rgb24)",
                other.name()
            )))
        }
    };
    if vf.width > u16::MAX as u32 || vf.height > u16::MAX as u32 {
        return Err(Error::unsupported("jpeg encode: dimensions exceed 65535"));
    }
    let (w, h) = (vf.width as usize, vf.height as usize);
    let row = w * channels;
    let stride = vf.strides[0];

    // jpeg-encoder wants tightly packed rows.
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
    let encoder = Encoder::new(&mut out, DEFAULT_QUALITY);
    encoder
        .encode(&packed, vf.width as u16, vf.height as u16, color)
        .map_err(|e| Error::invalid(format!("jpeg encode: {e}")))?;
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn jpeg_encode_decode_roundtrips_approximately() {
        let (w, h) = (48u32, 32u32);
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
        let src = VideoFrame {
            width: w,
            height: h,
            format: PixelFormat::Rgb24,
            planes: vec![rgb.clone()],
            strides: vec![wi * 3],
            pts: None,
        };

        let bytes = encode_jpeg(&src).unwrap();
        assert_eq!(&bytes[0..3], &[0xFF, 0xD8, 0xFF]); // SOI marker

        let Frame::Video(decoded) = decode_jpeg(&bytes).unwrap() else {
            unreachable!()
        };
        assert_eq!((decoded.width, decoded.height), (w, h));
        assert_eq!(decoded.format, PixelFormat::Rgb24);
        // JPEG is lossy; a smooth gradient should stay close.
        let total: u64 = rgb
            .iter()
            .zip(&decoded.planes[0])
            .map(|(a, b)| (*a as i16 - *b as i16).unsigned_abs() as u64)
            .sum();
        let mean = total as f64 / (wi * hi * 3) as f64;
        assert!(mean < 12.0, "jpeg round-trip drifted too far: {mean:.2}");
    }
}
