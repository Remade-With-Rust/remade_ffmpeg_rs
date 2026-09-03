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
//! feeds this decoder directly. `rusty_h264`'s `default-features` are off here
//! (no process-wide allocator, the portable scalar kernels); turn on this
//! crate's `asm` feature for its portable Rust SIMD (SSE2/AVX2/NEON — no
//! assembler, no C).
//!
//! # Encoder options
//!
//! `configure` reads the FFmpeg-shaped options the CLI forwards:
//!
//! - `-preset fast|medium|slow` — x264's ladder collapses onto `rusty_h264`'s
//!   three: `ultrafast`..`fast` → `Fast`, `medium` → `Balanced`,
//!   `slow`..`placebo` → `Quality`.
//! - `-profile baseline|main|high`. `baseline` is **Constrained Baseline**:
//!   CAVLC, no 8×8 transform, no B-frames, one reference, no lookahead, no
//!   scene cut — the configuration a chip runs (`rusty_esp_video`'s
//!   `chip_config`), so an `rff` encode on the host reproduces a device's
//!   stream byte for byte at the same preset, GOP, bitrate and QP.
//! - `-g N` (keyframe interval, also the minimum), `-b:v RATE`, `-qp Q`.
//!
//! # Buffering
//!
//! `rusty_h264` 0.12 runs a lookahead by default (mb-tree over a GOP): an
//! `encode` call may return nothing, then a whole GOP of access units at once,
//! and `flush` returns the tail. The adapter splits those into one packet per
//! access unit and carries each frame's `pts` through the delay in order (no
//! B-frames, so coding order is presentation order). Baseline turns the
//! lookahead off, so it is one packet per frame with no delay.

use std::collections::VecDeque;

use rff_codec::{Codec, CodecParams, CodecRegistry, Decoder, Encoder};
use rff_core::{Dictionary, Error, Frame, MediaType, Packet, PixelFormat, Result, VideoFrame};
use rusty_h264::{
    Decoder as RustyDecoder, Encoder as RustyEncoder, EncoderConfig, Preset, Profile, YuvFrame,
};
use std::panic::{catch_unwind, AssertUnwindSafe};

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

/// Split an Annex-B stream into access units: each ends after a VCL
/// (coded-slice) NAL, with the parameter-set / SEI NALs before it attached.
/// `rusty_h264` codes one slice per picture and writes no AUDs, so this is
/// exact for its output (it mirrors `rusty_h264_decoder::split_access_units`).
fn split_access_units(stream: &[u8]) -> Vec<&[u8]> {
    let mut codes: Vec<(usize, bool)> = Vec::new();
    let mut i = 0;
    while let Some(w) = stream.get(i..i + 3) {
        if w[0] == 0 && w[1] == 0 && w[2] == 1 {
            let nal_type = stream.get(i + 3).copied().unwrap_or(0) & 0x1f;
            let is_vcl = matches!(nal_type, 1 | 5);
            let sc = if i > 0 && stream.get(i - 1) == Some(&0) {
                i - 1
            } else {
                i
            };
            codes.push((sc, is_vcl));
            i += 3;
        } else {
            i += 1;
        }
    }
    if codes.is_empty() {
        return vec![stream];
    }
    let mut aus: Vec<&[u8]> = Vec::new();
    let mut start = codes[0].0;
    for k in 0..codes.len() {
        if codes[k].1 {
            let end = codes.get(k + 1).map_or(stream.len(), |c| c.0);
            aus.push(&stream[start..end]);
            start = end;
        }
    }
    if start < stream.len() {
        // Trailing non-VCL NALs (a parameter set with no picture yet): keep
        // them with the last unit rather than losing them.
        match aus.pop() {
            Some(last) => {
                let begin = last.as_ptr() as usize - stream.as_ptr() as usize;
                aus.push(&stream[begin..]);
            }
            None => aus.push(stream),
        }
    }
    aus
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

        // Defense in depth: the decoder eats attacker-controlled bytes. It is fuzzed to
        // never panic, but a bug here must never take down the host app — so catch any
        // unwind, reset the decoder, and surface a decode error instead of crashing.
        let inner = &mut self.inner;
        let frames = match catch_unwind(AssertUnwindSafe(|| inner.decode_stream(data))) {
            Ok(Ok(frames)) => frames,
            Ok(Err(e)) => return Err(map_err(e)),
            Err(_) => {
                self.inner = RustyDecoder::new();
                self.started = false;
                return Err(Error::InvalidData(
                    "rusty_h264: decoder panicked on malformed input (recovered)".into(),
                ));
            }
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

/// What `configure` collected; applied when the first frame fixes the geometry.
#[derive(Debug, Clone, Copy, PartialEq, Default)]
struct Settings {
    preset: Option<Preset>,
    profile: Option<Profile>,
    /// `-g`: keyframe interval (and minimum).
    keyint: Option<u32>,
    /// `-b:v`, bits per second.
    bitrate: Option<u32>,
    /// `-qp` / `-crf`.
    qp: Option<u8>,
}

impl Settings {
    /// The chip configuration for Constrained Baseline:
    /// [`EncoderConfig::baseline`], the one constructor the host and the
    /// device (`rusty_esp_video::h264::chip_config`) share, so an rff encode
    /// with `-profile baseline -preset fast` is the byte-for-byte oracle for
    /// a device stream. The geometry is kept; `-preset`, `-g`, `-b:v` and
    /// `-qp` are applied on top afterwards.
    fn baseline(cfg: &mut EncoderConfig) {
        *cfg = EncoderConfig::baseline(cfg.width, cfg.height);
    }

    fn apply(&self, cfg: &mut EncoderConfig) {
        match self.profile {
            Some(Profile::ConstrainedBaseline | Profile::Baseline) => Self::baseline(cfg),
            Some(Profile::Main) => {
                cfg.profile = Profile::Main;
                cfg.transform_8x8 = false;
            }
            Some(other) => cfg.profile = other,
            None => {}
        }
        if let Some(p) = self.preset {
            cfg.preset = p;
        }
        if let Some(g) = self.keyint {
            cfg.gop_size = g;
            cfg.min_keyint = g;
        }
        if let Some(b) = self.bitrate {
            cfg.bitrate = b;
        }
        if let Some(q) = self.qp {
            cfg.qp = q;
        }
    }
}

struct H264Encoder {
    inner: Option<RustyEncoder>,
    settings: Settings,
    /// `pts` of every frame sent and not yet emitted, in coding order.
    pending_pts: VecDeque<Option<i64>>,
    queue: VecDeque<Packet>,
    eof: bool,
}

impl H264Encoder {
    fn new() -> H264Encoder {
        H264Encoder {
            inner: None,
            settings: Settings::default(),
            pending_pts: VecDeque::new(),
            queue: VecDeque::new(),
            eof: false,
        }
    }

    /// Turn a (possibly multi-AU) encoder return into packets.
    fn emit(&mut self, stream: &[u8]) {
        if stream.is_empty() {
            return;
        }
        for au in split_access_units(stream) {
            let mut packet = Packet::from_data(0, au.to_vec());
            packet.flags.keyframe = au_has_idr(au);
            packet.pts = self.pending_pts.pop_front().flatten();
            packet.dts = packet.pts;
            self.queue.push_back(packet);
        }
    }
}

fn parse_preset(v: &str) -> Result<Preset> {
    Ok(match v.trim().to_ascii_lowercase().as_str() {
        "ultrafast" | "superfast" | "veryfast" | "faster" | "fast" => Preset::Fast,
        "medium" | "balanced" => Preset::Balanced,
        "slow" | "slower" | "veryslow" | "placebo" | "quality" => Preset::Quality,
        _ => {
            return Err(Error::unsupported(format!(
                "h264 encode: -preset `{v}` (use fast|medium|slow)"
            )))
        }
    })
}

fn parse_profile(v: &str) -> Result<Profile> {
    Ok(match v.trim().to_ascii_lowercase().as_str() {
        "baseline" | "constrained_baseline" | "constrained-baseline" => {
            Profile::ConstrainedBaseline
        }
        "main" => Profile::Main,
        "high" => Profile::High,
        _ => {
            return Err(Error::unsupported(format!(
                "h264 encode: -profile `{v}` (use baseline|main|high)"
            )))
        }
    })
}

impl Encoder for H264Encoder {
    fn configure(&mut self, options: &Dictionary) -> Result<()> {
        if let Some(v) = options.get("preset") {
            self.settings.preset = Some(parse_preset(v)?);
        }
        if let Some(v) = options.get("profile") {
            self.settings.profile = Some(parse_profile(v)?);
        }
        if let Some(v) = options.get("g") {
            let g = v
                .trim()
                .parse::<u32>()
                .ok()
                .filter(|g| *g > 0)
                .ok_or_else(|| {
                    Error::unsupported(format!(
                        "h264 encode: -g wants a positive frame count, got `{v}`"
                    ))
                })?;
            self.settings.keyint = Some(g);
        }
        if let Some(b) = options.get_bitrate("b") {
            self.settings.bitrate = Some(b.clamp(0, i64::from(u32::MAX)) as u32);
        }
        for key in ["qp", "crf"] {
            if let Some(v) = options.get(key) {
                let q = v
                    .trim()
                    .parse::<u8>()
                    .ok()
                    .filter(|q| *q <= 51)
                    .ok_or_else(|| {
                        Error::unsupported(format!("h264 encode: -{key} wants 0..51, got `{v}`"))
                    })?;
                self.settings.qp = Some(q);
            }
        }
        Ok(())
    }

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

        if self.inner.is_none() {
            let mut cfg = EncoderConfig::new(w, h);
            self.settings.apply(&mut cfg);
            self.inner = Some(RustyEncoder::new(cfg).map_err(map_err)?);
        }
        let enc = self.inner.as_mut().expect("created above");
        self.pending_pts.push_back(vf.pts);
        let out = enc.try_encode(&yuv).map_err(map_err)?;
        self.emit(&out);
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
        if let Some(enc) = self.inner.as_mut() {
            let tail = enc.flush();
            self.emit(&tail);
        }
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

    fn grey_frame(w: u32, h: u32, luma: u8, pts: i64) -> Frame {
        let (wi, hi) = (w as usize, h as usize);
        Frame::Video(VideoFrame {
            width: w,
            height: h,
            format: PixelFormat::Yuv420p,
            planes: vec![
                vec![luma; wi * hi],
                vec![128u8; (wi / 2) * (hi / 2)],
                vec![128u8; (wi / 2) * (hi / 2)],
            ],
            strides: vec![wi, wi / 2, wi / 2],
            pts: Some(pts),
        })
    }

    fn drain(enc: &mut H264Encoder) -> Vec<Packet> {
        let mut v = Vec::new();
        while let Ok(p) = enc.receive_packet() {
            v.push(p);
        }
        v
    }

    /// `(profile_idc, constraint_set flags)` from the SPS in an access unit.
    fn sps_profile(au: &[u8]) -> (u8, u8) {
        let mut i = 0;
        while i + 5 < au.len() {
            if au[i] == 0 && au[i + 1] == 0 && au[i + 2] == 1 && au[i + 3] & 0x1f == 7 {
                return (au[i + 4], au[i + 5]);
            }
            i += 1;
        }
        panic!("no SPS in the access unit");
    }

    #[test]
    fn encode_decode_roundtrip() {
        let (w, h) = (64u32, 48u32);
        let mut enc = H264Encoder::new();
        enc.send_frame(&grey_frame(w, h, 128, 0)).unwrap();
        enc.flush();
        let packets = drain(&mut enc);
        assert_eq!(packets.len(), 1);
        let packet = &packets[0];
        assert!(!packet.data.is_empty());
        assert!(packet.flags.keyframe, "first frame must be an IDR");
        assert_eq!(packet.pts, Some(0));

        let mut dec = H264Decoder::new();
        dec.send_packet(packet).unwrap();
        dec.flush();
        match dec.receive_frame().unwrap() {
            Frame::Video(v) => {
                assert_eq!((v.width, v.height), (w, h));
                assert_eq!(v.format, PixelFormat::Yuv420p);
            }
            Frame::Audio(_) => panic!("expected a video frame"),
        }
    }

    /// The default configuration runs a lookahead: frames buffer, then come
    /// out together. Every frame must still become exactly one packet, in
    /// order, with its own `pts`.
    #[test]
    fn lookahead_buffering_yields_one_packet_per_frame_with_its_pts() {
        let (w, h) = (64u32, 48u32);
        let mut enc = H264Encoder::new();
        let n = 12;
        for i in 0..n {
            enc.send_frame(&grey_frame(w, h, 100 + i as u8 * 5, i as i64 * 40))
                .unwrap();
        }
        enc.flush();
        let packets = drain(&mut enc);
        assert_eq!(packets.len(), n, "one packet per frame after flush");
        for (i, p) in packets.iter().enumerate() {
            assert_eq!(
                p.pts,
                Some(i as i64 * 40),
                "packet {i} keeps its frame's pts"
            );
            assert!(!p.data.is_empty());
        }
        assert!(packets[0].flags.keyframe);
        // The decoder agrees on the count.
        let mut dec = H264Decoder::new();
        for p in &packets {
            dec.send_packet(p).unwrap();
        }
        dec.flush();
        let mut frames = 0;
        while dec.receive_frame().is_ok() {
            frames += 1;
        }
        assert_eq!(frames, n);
    }

    /// `-profile baseline -preset fast -g 30`: the chip configuration. The
    /// stream must say Constrained Baseline (profile_idc 66 with
    /// constraint_set1) and come out one packet per frame with no delay.
    #[test]
    fn baseline_profile_is_constrained_baseline_and_unbuffered() {
        let mut opts = Dictionary::new();
        opts.set("profile", "baseline");
        opts.set("preset", "fast");
        opts.set("g", "30");
        opts.set("b", "500k");
        opts.set("qp", "28");
        let mut enc = H264Encoder::new();
        enc.configure(&opts).unwrap();
        for i in 0..3 {
            enc.send_frame(&grey_frame(64, 48, 90, i)).unwrap();
            let got = drain(&mut enc);
            assert_eq!(got.len(), 1, "frame {i}: no lookahead delay");
            if i == 0 {
                let (profile_idc, flags) = sps_profile(&got[0].data);
                assert_eq!(profile_idc, 66, "Baseline profile_idc");
                assert_ne!(flags & 0x40, 0, "constraint_set1_flag: Constrained");
            }
        }
        enc.flush();
        assert!(drain(&mut enc).is_empty());
        let cfg = enc.inner.as_ref().unwrap().config();
        assert_eq!(cfg.profile, Profile::ConstrainedBaseline);
        assert!(!cfg.cabac && !cfg.transform_8x8 && cfg.bframes == 0);
        assert_eq!((cfg.num_ref_frames, cfg.lookahead, cfg.scenecut), (1, 0, 0));
        assert_eq!((cfg.gop_size, cfg.min_keyint), (30, 30));
        assert_eq!(
            (cfg.bitrate, cfg.qp, cfg.preset),
            (500_000, 28, Preset::Fast)
        );
    }

    /// A frame with enough texture that every macroblock codes something.
    fn textured_frame(w: u32, h: u32, t: i64) -> Frame {
        let (wi, hi) = (w as usize, h as usize);
        let mut y = vec![0u8; wi * hi];
        for r in 0..hi {
            for c in 0..wi {
                y[r * wi + c] = ((c * 3 + r * 5 + (t as usize) * 11) ^ (r & 7) * 9) as u8;
            }
        }
        let (cw, ch) = (wi / 2, hi / 2);
        let u: Vec<u8> = (0..cw * ch)
            .map(|i| (96 + (i * 7 + t as usize * 3) % 64) as u8)
            .collect();
        let v: Vec<u8> = (0..cw * ch)
            .map(|i| (160 - (i * 5 + t as usize * 2) % 48) as u8)
            .collect();
        Frame::Video(VideoFrame {
            width: w,
            height: h,
            format: PixelFormat::Yuv420p,
            planes: vec![y, u, v],
            strides: vec![wi, cw, cw],
            pts: Some(t),
        })
    }

    /// `-profile baseline -preset fast`: an rff encode is the chip
    /// configuration byte for byte — the same bytes `rusty_h264` produces
    /// from `EncoderConfig::baseline` directly. This is the host oracle a
    /// device stream is compared against.
    #[test]
    fn baseline_encode_is_byte_identical_to_encoder_config_baseline() {
        let (w, h) = (64u32, 48u32);
        let frames: Vec<Frame> = (0..6).map(|t| textured_frame(w, h, t)).collect();

        let mut opts = Dictionary::new();
        opts.set("profile", "baseline");
        opts.set("preset", "fast");
        opts.set("g", "4");
        opts.set("qp", "26");
        let mut enc = H264Encoder::new();
        enc.configure(&opts).unwrap();
        let mut via_rff = Vec::new();
        for f in &frames {
            enc.send_frame(f).unwrap();
            for p in drain(&mut enc) {
                via_rff.extend_from_slice(&p.data);
            }
        }
        enc.flush();
        for p in drain(&mut enc) {
            via_rff.extend_from_slice(&p.data);
        }

        let mut cfg = EncoderConfig::baseline(w as usize, h as usize);
        cfg.gop_size = 4;
        cfg.min_keyint = 4;
        cfg.qp = 26;
        let mut direct = RustyEncoder::new(cfg).unwrap();
        let mut via_lib = Vec::new();
        for f in &frames {
            let Frame::Video(vf) = f else { unreachable!() };
            let yuv = YuvFrame {
                width: w as usize,
                height: h as usize,
                y: vf.planes[0].clone(),
                u: vf.planes[1].clone(),
                v: vf.planes[2].clone(),
            };
            via_lib.extend_from_slice(&direct.encode_planes(&yuv.as_planes()).unwrap());
        }
        via_lib.extend_from_slice(&direct.flush());

        assert!(via_rff.len() > 6 * 8, "the stream has bytes");
        assert_eq!(via_rff, via_lib, "rff baseline != EncoderConfig::baseline");
    }

    #[test]
    fn bad_options_are_refused_up_front() {
        let mut enc = H264Encoder::new();
        let mut opts = Dictionary::new();
        opts.set("preset", "warp");
        assert!(enc.configure(&opts).is_err());
        let mut opts = Dictionary::new();
        opts.set("profile", "extended");
        assert!(enc.configure(&opts).is_err());
        let mut opts = Dictionary::new();
        opts.set("qp", "99");
        assert!(enc.configure(&opts).is_err());
        let mut opts = Dictionary::new();
        opts.set("preset", "veryslow");
        opts.set("profile", "high");
        assert!(enc.configure(&opts).is_ok());
        assert_eq!(enc.settings.preset, Some(Preset::Quality));
    }

    #[test]
    fn access_unit_splitter_keeps_parameter_sets_with_their_picture() {
        let mut s = Vec::new();
        for nal in [
            &[0x67u8, 1][..],
            &[0x68, 2],
            &[0x65, 3, 4],
            &[0x41, 5],
            &[0x67, 6],
        ] {
            s.extend_from_slice(&[0, 0, 0, 1]);
            s.extend_from_slice(nal);
        }
        let aus = split_access_units(&s);
        assert_eq!(aus.len(), 2);
        assert_eq!(aus[0], &s[..19], "SPS + PPS + IDR");
        assert_eq!(
            aus[1],
            &s[19..],
            "P slice, then the trailing SPS stays attached"
        );
        assert_eq!(split_access_units(b"junk"), vec![&b"junk"[..]]);
    }

    #[test]
    fn malformed_packets_never_crash() {
        // A decoder eats attacker-controlled bytes; send_packet must return Ok/Err for
        // ANY input, never unwind out of the codec. The catch_unwind boundary makes even
        // a hypothetical decoder panic a recoverable error (and resets the decoder).
        let mut rng = 0x1234_5678_9abc_def0u64;
        let mut next = || {
            rng ^= rng << 13;
            rng ^= rng >> 7;
            rng ^= rng << 17;
            rng
        };
        let mut dec = H264Decoder::new();
        for _ in 0..4000 {
            let len = (next() % 256) as usize;
            let data: Vec<u8> = (0..len).map(|_| next() as u8).collect();
            // A panic here would unwind and fail the test; a graceful Ok/Err passes.
            let _ = dec.send_packet(&Packet::from_data(0, data));
            let _ = dec.receive_frame();
        }
    }
}
