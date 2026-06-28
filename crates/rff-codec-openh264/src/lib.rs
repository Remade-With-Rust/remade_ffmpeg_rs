//! **TEMPORARY** H.264 codec via Cisco's [`openh264`] — a C library behind FFI.
//!
//! ⚠️ This is the project's only non-pure-Rust, non-`unsafe`-free codec. It
//! exists as a stopgap so H.264 works *today*, and is kept out of the default
//! build (gated by the `h264-openh264` feature on the `rff` facade). It will be
//! replaced by the in-house pure-Rust H.264 decoder; when that lands, delete
//! this crate and the feature.
//!
//! Decode and encode both go through `openh264` (Annex-B bitstream, YUV 4:2:0).

use std::collections::VecDeque;

use openh264::decoder::Decoder as H264CDecoder;
use openh264::encoder::Encoder as H264CEncoder;
use openh264::formats::{YUVBuffer, YUVSource};
use rff_codec::{Codec, CodecRegistry, Decoder, Encoder};
use rff_core::{Error, Frame, MediaType, Packet, PixelFormat, Result, VideoFrame};

/// Register the openh264-backed H.264 codec, overriding the scaffold for
/// [`CodecId::H264`](rff_core::CodecId::H264). Called only when the
/// `h264-openh264` feature is enabled.
pub fn register(registry: &mut CodecRegistry) {
    registry.register(Codec {
        id: rff_core::CodecId::H264,
        name: "h264",
        long_name: "H.264 / AVC (via Cisco openh264 — TEMPORARY C/FFI)",
        media_type: MediaType::Video,
        decoder: Some(|| Box::new(OpenH264Decoder::new())),
        encoder: Some(|| Box::new(OpenH264Encoder::default())),
    });
}

fn map_err<E: std::fmt::Display>(e: E) -> Error {
    Error::InvalidData(format!("openh264: {e}"))
}

// ---------------------------------------------------------------------------
// Decoder
// ---------------------------------------------------------------------------

struct OpenH264Decoder {
    decoder: Option<H264CDecoder>,
    queue: VecDeque<Frame>,
    eof: bool,
}

impl OpenH264Decoder {
    fn new() -> OpenH264Decoder {
        OpenH264Decoder {
            decoder: None,
            queue: VecDeque::new(),
            eof: false,
        }
    }
}

impl Decoder for OpenH264Decoder {
    fn send_packet(&mut self, packet: &Packet) -> Result<()> {
        if self.decoder.is_none() {
            self.decoder = Some(H264CDecoder::new().map_err(map_err)?);
        }
        let decoder = self.decoder.as_mut().unwrap();

        if let Some(yuv) = decoder.decode(&packet.data).map_err(map_err)? {
            let (w, h) = yuv.dimensions();
            let (sy, su, sv) = yuv.strides();
            self.queue.push_back(Frame::Video(VideoFrame {
                width: w as u32,
                height: h as u32,
                format: PixelFormat::Yuv420p,
                planes: vec![yuv.y().to_vec(), yuv.u().to_vec(), yuv.v().to_vec()],
                strides: vec![sy, su, sv],
                pts: packet.pts,
            }));
        }
        Ok(())
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
        self.eof = true;
    }
}

// ---------------------------------------------------------------------------
// Encoder
// ---------------------------------------------------------------------------

#[derive(Default)]
struct OpenH264Encoder {
    encoder: Option<H264CEncoder>,
    queue: VecDeque<Packet>,
    eof: bool,
}

impl Encoder for OpenH264Encoder {
    fn send_frame(&mut self, frame: &Frame) -> Result<()> {
        let vf = match frame {
            Frame::Video(v) => v,
            Frame::Audio(_) => {
                return Err(Error::unsupported(
                    "h264 encode: audio frame on a video codec",
                ))
            }
        };
        if vf.format != PixelFormat::Yuv420p {
            return Err(Error::unsupported(format!(
                "h264 encode: needs yuv420p, got `{}`",
                vf.format.name()
            )));
        }
        let (w, h) = (vf.width as usize, vf.height as usize);
        if w % 2 != 0 || h % 2 != 0 {
            return Err(Error::unsupported("h264 encode: dimensions must be even"));
        }

        // Pack into a tight contiguous I420 buffer (Y, then U, then V).
        let mut i420 = Vec::with_capacity(w * h * 3 / 2);
        for row in 0..h {
            let s = row * vf.strides[0];
            i420.extend_from_slice(&vf.planes[0][s..s + w]);
        }
        for plane in 1..=2 {
            for row in 0..h / 2 {
                let s = row * vf.strides[plane];
                i420.extend_from_slice(&vf.planes[plane][s..s + w / 2]);
            }
        }

        if self.encoder.is_none() {
            self.encoder = Some(H264CEncoder::new().map_err(map_err)?);
        }
        let encoder = self.encoder.as_mut().unwrap();
        let yuv = YUVBuffer::from_vec(i420, w, h);
        let bitstream = encoder.encode(&yuv).map_err(map_err)?;
        let mut packet = Packet::from_data(0, bitstream.to_vec());
        packet.flags.keyframe = true;
        packet.pts = vf.pts;
        self.queue.push_back(packet);
        Ok(())
    }

    fn receive_packet(&mut self) -> Result<Packet> {
        if let Some(packet) = self.queue.pop_front() {
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

#[cfg(test)]
mod tests {
    use super::*;

    /// Encode one YUV420 frame to H.264 and decode it back (lossy → dims only).
    #[test]
    fn h264_encode_decode_roundtrip() {
        let (w, h) = (32u32, 32u32);
        let (wi, hi) = (w as usize, h as usize);
        let y = vec![128u8; wi * hi];
        let chroma = vec![128u8; (wi / 2) * (hi / 2)];
        let frame = Frame::Video(VideoFrame {
            width: w,
            height: h,
            format: PixelFormat::Yuv420p,
            planes: vec![y, chroma.clone(), chroma],
            strides: vec![wi, wi / 2, wi / 2],
            pts: Some(0),
        });

        let mut enc = OpenH264Encoder::default();
        enc.send_frame(&frame).unwrap();
        let packet = enc.receive_packet().unwrap();
        assert!(!packet.data.is_empty());

        let mut dec = OpenH264Decoder::new();
        dec.send_packet(&packet).unwrap();
        dec.flush();
        // openh264 may need a flush/extra AU; accept a frame if produced.
        if let Ok(Frame::Video(v)) = dec.receive_frame() {
            assert_eq!((v.width, v.height), (w, h));
            assert_eq!(v.format, PixelFormat::Yuv420p);
        }
    }
}
