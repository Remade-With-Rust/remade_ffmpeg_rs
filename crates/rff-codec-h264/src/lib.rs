//! H.264 / AVC video codec, backed by the pure-Rust
//! [`rusty_h264`](https://crates.io/crates/rusty_h264) encoder + decoder.
//!
//! This is the **default** H.264 implementation: `register` is wired into the
//! `rff` facade unconditionally, so `-c:v h264` decodes and encodes through
//! `rusty_h264` with no C and no FFI. (The C `openh264` path still exists behind
//! the opt-in `h264-openh264` feature and overrides this when enabled.)
//!
//! Bitstream is **Annex-B**, pixels are **YUV 4:2:0** — the same shape the
//! openh264 path uses, so the MP4 demuxer's AVCC→Annex-B conversion upstream
//! feeds this decoder directly. `rusty_h264`'s `default-features` are off here, so
//! the build stays 100% safe Rust with no `nasm`; turn on this crate's `asm`
//! feature for the vendored SIMD speedup.

use std::collections::VecDeque;

use rff_codec::{Codec, CodecParams, CodecRegistry, Decoder, Encoder};
use rff_core::{Error, Frame, MediaType, Packet, PixelFormat, Result, VideoFrame};
use rusty_h264::{Decoder as RustyDecoder, Encoder as RustyEncoder, EncoderConfig, YuvFrame};

/// Register the pure-Rust H.264 codec into a [`CodecRegistry`].
pub fn register(registry: &mut CodecRegistry) {
    registry.register(Codec {
        id: rff_core::CodecId::H264,
        name: "h264",
        long_name: "H.264 / AVC / MPEG-4 AVC (pure-Rust rusty_h264)",
        media_type: MediaType::Video,
        decoder: Some(|| Box::new(H264Decoder::new())),
        encoder: Some(|| Box::new(H264Encoder::new())),
    });
}

fn map_err<E: std::fmt::Display>(e: E) -> Error {
    Error::InvalidData(format!("rusty_h264: {e}"))
}

/// True if `data` begins with a 3- or 4-byte Annex-B start code.
fn is_annex_b(data: &[u8]) -> bool {
    data.starts_with(&[0, 0, 1]) || data.starts_with(&[0, 0, 0, 1])
}

/// Scan an Annex-B access unit for an IDR slice NAL (`nal_unit_type == 5`).
fn au_has_idr(data: &[u8]) -> bool {
    let mut i = 0;
    while i + 3 < data.len() {
        if data[i] == 0 && data[i + 1] == 0 && data[i + 2] == 1 {
            if (data[i + 3] & 0x1f) == 5 {
                return true;
            }
            i += 3;
        } else {
            i += 1;
        }
    }
    false
}

// ---------------------------------------------------------------------------
// Decoder
// ---------------------------------------------------------------------------

struct H264Decoder {
    inner: RustyDecoder,
    /// Out-of-band SPS/PPS (from `extradata`), prepended to the first packet if
    /// it is itself Annex-B. AVCC `avcC` extradata is handled by the demuxer.
    extradata: Vec<u8>,
    started: bool,
    queue: VecDeque<Frame>,
    eof: bool,
}

impl H264Decoder {
    fn new() -> H264Decoder {
        H264Decoder {
            inner: RustyDecoder::new(),
            extradata: Vec::new(),
            started: false,
            queue: VecDeque::new(),
            eof: false,
        }
    }
}

impl Decoder for H264Decoder {
    fn configure(&mut self, params: &CodecParams) -> Result<()> {
        if is_annex_b(&params.extradata) {
            self.extradata = params.extradata.clone();
        }
        Ok(())
    }

    fn send_packet(&mut self, packet: &Packet) -> Result<()> {
        // Prepend Annex-B SPS/PPS once, ahead of the first coded packet.
        let frames = if !self.started && !self.extradata.is_empty() {
            self.started = true;
            let mut au = std::mem::take(&mut self.extradata);
            au.extend_from_slice(&packet.data);
            self.inner.decode_stream(&au).map_err(map_err)?
        } else {
            self.started = true;
            self.inner.decode_stream(&packet.data).map_err(map_err)?
        };

        for yuv in frames {
            self.queue.push_back(yuv_to_frame(yuv, packet.pts));
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

/// Map a `rusty_h264` [`YuvFrame`] (tight I420 planes) to an rff [`VideoFrame`].
fn yuv_to_frame(f: YuvFrame, pts: Option<i64>) -> Frame {
    let (w, h) = (f.width, f.height);
    Frame::Video(VideoFrame {
        width: w as u32,
        height: h as u32,
        format: PixelFormat::Yuv420p,
        planes: vec![f.y, f.u, f.v],
        strides: vec![w, w / 2, w / 2],
        pts,
    })
}

// ---------------------------------------------------------------------------
// Encoder
// ---------------------------------------------------------------------------

struct H264Encoder {
    inner: Option<RustyEncoder>,
    queue: VecDeque<Packet>,
    eof: bool,
}

impl H264Encoder {
    fn new() -> H264Encoder {
        H264Encoder {
            inner: None,
            queue: VecDeque::new(),
            eof: false,
        }
    }
}

impl Encoder for H264Encoder {
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
        if w == 0 || h == 0 || w % 2 != 0 || h % 2 != 0 {
            return Err(Error::unsupported(
                "h264 encode: dimensions must be even and non-zero",
            ));
        }

        // rusty_h264 wants tight (stride == width) planes; copy row by row to
        // strip any padding the upstream frame carries.
        let yuv = YuvFrame {
            width: w,
            height: h,
            y: tighten(&vf.planes[0], vf.strides[0], w, h),
            u: tighten(&vf.planes[1], vf.strides[1], w / 2, h / 2),
            v: tighten(&vf.planes[2], vf.strides[2], w / 2, h / 2),
        };

        let enc = match self.inner {
            Some(ref mut e) => e,
            None => {
                let cfg = EncoderConfig::new(w, h);
                self.inner = Some(RustyEncoder::new(cfg).map_err(map_err)?);
                self.inner.as_mut().unwrap()
            }
        };

        let au = enc.encode(&yuv);
        let mut packet = Packet::from_data(0, au);
        packet.flags.keyframe = au_has_idr(&packet.data);
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

/// Copy `rows` rows of `width` bytes out of a (possibly padded) plane into a
/// tight, contiguous `width * rows` buffer.
fn tighten(plane: &[u8], stride: usize, width: usize, rows: usize) -> Vec<u8> {
    if stride == width {
        return plane[..width * rows].to_vec();
    }
    let mut out = Vec::with_capacity(width * rows);
    for r in 0..rows {
        let s = r * stride;
        out.extend_from_slice(&plane[s..s + width]);
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn encode_decode_roundtrip() {
        let (w, h) = (64u32, 48u32);
        let (wi, hi) = (w as usize, h as usize);
        let frame = Frame::Video(VideoFrame {
            width: w,
            height: h,
            format: PixelFormat::Yuv420p,
            planes: vec![
                vec![128u8; wi * hi],
                vec![128u8; (wi / 2) * (hi / 2)],
                vec![128u8; (wi / 2) * (hi / 2)],
            ],
            strides: vec![wi, wi / 2, wi / 2],
            pts: Some(0),
        });

        let mut enc = H264Encoder::new();
        enc.send_frame(&frame).unwrap();
        let packet = enc.receive_packet().unwrap();
        assert!(!packet.data.is_empty());
        assert!(packet.flags.keyframe, "first frame must be an IDR");

        let mut dec = H264Decoder::new();
        dec.send_packet(&packet).unwrap();
        dec.flush();
        match dec.receive_frame().unwrap() {
            Frame::Video(v) => {
                assert_eq!((v.width, v.height), (w, h));
                assert_eq!(v.format, PixelFormat::Yuv420p);
            }
            Frame::Audio(_) => panic!("expected a video frame"),
        }
    }
}
