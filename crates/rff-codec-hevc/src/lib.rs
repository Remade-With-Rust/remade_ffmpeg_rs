//! HEVC / H.265 video decoding, backed by the pure-Rust
//! [`rusty_h265`](https://crates.io/crates/rusty_h265) decoder.
//!
//! Bitstream is **Annex-B**, pixels are **YUV 4:2:0** (8 or 10 bit) — the same
//! shape the H.264 path uses, so the MP4/Matroska demuxers' `hvcC`→Annex-B
//! conversion feeds this decoder directly. Decode only: an HEVC *encoder* is a
//! separate mission (see `docs/plans/rusty_hevc.md`).
//!
//! Scope is HEVC version 1 — Main, Main 10 and Main Still Picture — which is
//! what phones, cameras, broadcast and Blu-ray produce. Range extensions,
//! screen content coding and the layered profiles are refused by name rather
//! than decoded wrongly.

use std::collections::VecDeque;
use std::panic::{catch_unwind, AssertUnwindSafe};

use rff_codec::{Codec, CodecParams, CodecRegistry, Decoder};
use rff_core::{Error, Frame, MediaType, Packet, PixelFormat, Result, VideoFrame};
use rusty_h265::Decoder as RustyDecoder;

/// Register the pure-Rust HEVC decoder into a [`CodecRegistry`].
pub fn register(registry: &mut CodecRegistry) {
    registry.register(Codec {
        id: rff_core::CodecId::Hevc,
        name: "hevc",
        long_name: "H.265 / HEVC (pure-Rust rusty_h265)",
        media_type: MediaType::Video,
        decoder: Some(|| Box::new(HevcDecoder::new())),
        encoder: None,
    });
}

fn map_err(e: rusty_h265::Error) -> Error {
    match e {
        rusty_h265::Error::Unsupported(m) => Error::Unsupported(format!("hevc: {m}")),
        other => Error::InvalidData(format!("rusty_h265: {other}")),
    }
}

/// True if `data` begins with a 3- or 4-byte Annex-B start code.
fn is_annex_b(data: &[u8]) -> bool {
    data.starts_with(&[0, 0, 1]) || data.starts_with(&[0, 0, 0, 1])
}

struct HevcDecoder {
    inner: RustyDecoder,
    /// Out-of-band VPS/SPS/PPS (from `extradata`), prepended to the first
    /// packet when it is itself Annex-B. `hvcC` extradata is unpacked by the
    /// demuxer, which hands us Annex-B.
    extradata: Vec<u8>,
    started: bool,
    queue: VecDeque<Frame>,
    eof: bool,
}

impl HevcDecoder {
    fn new() -> HevcDecoder {
        HevcDecoder {
            inner: RustyDecoder::new(),
            extradata: Vec::new(),
            started: false,
            queue: VecDeque::new(),
            eof: false,
        }
    }

    fn drain(&mut self) {
        while let Ok(f) = self.inner.next_frame() {
            self.queue.push_back(frame_to_rff(&f));
        }
    }
}

impl Decoder for HevcDecoder {
    fn configure(&mut self, params: &CodecParams) -> Result<()> {
        if is_annex_b(&params.extradata) {
            self.extradata = params.extradata.clone();
        }
        Ok(())
    }

    fn send_packet(&mut self, packet: &Packet) -> Result<()> {
        let owned_au;
        let data: &[u8] = if !self.started && !self.extradata.is_empty() {
            self.started = true;
            let mut au = std::mem::take(&mut self.extradata);
            au.extend_from_slice(&packet.data);
            owned_au = au;
            &owned_au
        } else {
            self.started = true;
            &packet.data
        };

        // The decoder eats attacker-controlled bytes; it is `forbid(unsafe_code)`
        // and fuzzed, but a bug here must never take down the host application.
        let inner = &mut self.inner;
        let pts = packet.pts;
        let r = catch_unwind(AssertUnwindSafe(|| inner.push_annexb(data, pts)));
        match r {
            Ok(Ok(())) => {}
            Ok(Err(e)) => {
                self.drain();
                return Err(map_err(e));
            }
            Err(_) => {
                self.inner = RustyDecoder::new();
                self.started = false;
                return Err(Error::InvalidData(
                    "rusty_h265: decoder panicked on malformed input (recovered)".into(),
                ));
            }
        }
        self.drain();
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
        if !self.eof {
            self.inner.flush();
            self.drain();
        }
        self.eof = true;
    }
}

/// Copies a decoded picture into rff's frame shape, cropping to the
/// conformance window and narrowing 8-bit samples to bytes.
fn frame_to_rff(f: &rusty_h265::Frame) -> Frame {
    let pic = &f.picture;
    let (cx, cy, cw, ch) = pic.crop;
    let wide = pic.bit_depth_luma > 8 || pic.bit_depth_chroma > 8;
    let (sw, sh) = match pic.chroma_format_idc {
        1 => (2usize, 2usize),
        2 => (2, 1),
        _ => (1, 1),
    };
    let mut planes = Vec::with_capacity(3);
    let mut strides = Vec::with_capacity(3);
    for (i, plane) in pic.planes.iter().enumerate() {
        let (x0, y0, w, h) = if i == 0 { (cx, cy, cw, ch) } else { (cx / sw, cy / sh, cw / sw, ch / sh) };
        let mut out = Vec::with_capacity(w * h * if wide { 2 } else { 1 });
        for y in y0..y0 + h {
            let row = &plane.data[y * plane.stride + x0..y * plane.stride + x0 + w];
            if wide {
                for &v in row {
                    out.extend_from_slice(&v.to_le_bytes());
                }
            } else {
                out.extend(row.iter().map(|&v| v as u8));
            }
        }
        strides.push(w * if wide { 2 } else { 1 });
        planes.push(out);
    }
    let format = match (pic.chroma_format_idc, wide) {
        (1, false) => PixelFormat::Yuv420p,
        (1, true) => PixelFormat::Yuv420p10,
        (2, false) => PixelFormat::Yuv422p,
        (2, true) => PixelFormat::Yuv422p10,
        (3, false) => PixelFormat::Yuv444p,
        _ => PixelFormat::Yuv444p10,
    };
    Frame::Video(VideoFrame {
        width: f.width as u32,
        height: f.height as u32,
        format,
        planes,
        strides,
        pts: f.pts,
    })
}