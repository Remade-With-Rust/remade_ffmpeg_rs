//! In-house **VP9 codec**, backed by the pure-Rust
//! [`rusty_vp9`] decoder + encoder — no C, no FFI.
//!
//! This crate is the thin `rff` adapter: [`register`] wires `rusty_vp9` into a
//! [`CodecRegistry`], private wrappers translate between the rff
//! `Decoder`/`Encoder` traits and `rusty_vp9`'s native push/pull API
//! (`Packet`/`Frame` ↔ raw bytes/planes, `Dictionary` options ↔
//! [`Vp9EncoderConfig`], and the `Again`/`Eof` control-flow errors one-to-one).
//! All codec logic — the decoder that is bit-exact against all 315 official
//! libvpx conformance vectors, and the encoder validated pixel-exact against
//! libvpx/ffmpeg — lives in `rusty_vp9`.

use rff_codec::{Codec, CodecParams, CodecRegistry, Decoder, Encoder};
use rff_core::{
    CodecId, Dictionary, Error, Frame, MediaType, Packet, PixelFormat, Result, VideoFrame,
};
use rusty_vp9::{DecodedFrame, Vp9EncoderConfig};

// The full engine, for callers that want the native API directly.
pub use rusty_vp9;
// The instruments and bitstream tools the `video-tests` analyzer and this
// crate's examples drive in-process (same surface the pre-split crate exported).
pub use rusty_vp9::{
    consume_compressed_header, encode_prof, parse_uncompressed_header, prof, ref_hist_take,
    set_modemap_std, set_snap_pool, BitReader, BoolDecoder, FrameHeader,
};

/// Register the VP9 decoder + encoder into a [`CodecRegistry`].
pub fn register(registry: &mut CodecRegistry) {
    registry.register(Codec {
        id: CodecId::Vp9,
        name: "vp9",
        long_name: "Google VP9",
        media_type: MediaType::Video,
        decoder: Some(|| Box::new(Vp9Decoder::default())),
        encoder: Some(|| Box::new(Vp9Encoder::default())),
    });
}

/// Map a `rusty_vp9` error onto the rff error space. `Again`/`Eof` are
/// control flow (FFmpeg's `EAGAIN`/`EOF` convention) and must map exactly —
/// the send/receive loops key on them.
fn map_err(e: rusty_vp9::Error) -> Error {
    match e {
        rusty_vp9::Error::Again => Error::Again,
        rusty_vp9::Error::Eof => Error::Eof,
        rusty_vp9::Error::Unimplemented(what) => Error::Unimplemented(what),
        rusty_vp9::Error::InvalidData(msg) => Error::InvalidData(msg),
        rusty_vp9::Error::Unsupported(msg) => Error::Unsupported(msg),
        // `rusty_vp9::Error` is `#[non_exhaustive]`; surface anything new as a
        // decode error rather than silently swallowing it.
        other => Error::InvalidData(format!("vp9: {other}")),
    }
}

// ---------------------------------------------------------------------------
// Decoder
// ---------------------------------------------------------------------------

#[derive(Default)]
struct Vp9Decoder {
    inner: rusty_vp9::Vp9Decoder,
}

impl Decoder for Vp9Decoder {
    fn configure(&mut self, _params: &CodecParams) -> Result<()> {
        // The VP9 bitstream carries its own size/colour configuration; the
        // container's hints are not needed.
        Ok(())
    }

    fn send_packet(&mut self, packet: &Packet) -> Result<()> {
        self.inner.push(&packet.data, packet.pts).map_err(map_err)
    }

    fn receive_frame(&mut self) -> Result<Frame> {
        let f = self.inner.next_frame().map_err(map_err)?;
        Ok(decoded_to_frame(f))
    }

    fn flush(&mut self) {
        self.inner.flush();
    }
}

/// Map a `rusty_vp9` [`DecodedFrame`] (subsampling + bit depth) onto an rff
/// [`VideoFrame`] with the matching [`PixelFormat`].
fn decoded_to_frame(f: DecodedFrame) -> Frame {
    let format = match (f.subsampling_x, f.subsampling_y, f.bit_depth) {
        (1, 1, 8) => PixelFormat::Yuv420p,
        (1, 0, 8) => PixelFormat::Yuv422p,
        (_, _, 8) => PixelFormat::Yuv444p,
        (1, 1, 10) => PixelFormat::Yuv420p10,
        (1, 0, 10) => PixelFormat::Yuv422p10,
        (_, _, 10) => PixelFormat::Yuv444p10,
        (1, 1, _) => PixelFormat::Yuv420p12,
        (1, 0, _) => PixelFormat::Yuv422p12,
        (_, _, _) => PixelFormat::Yuv444p12,
    };
    Frame::Video(VideoFrame {
        width: f.width,
        height: f.height,
        format,
        planes: f.planes,
        strides: f.strides,
        pts: f.pts,
    })
}

// ---------------------------------------------------------------------------
// Encoder
// ---------------------------------------------------------------------------

/// Parse an ffmpeg-style bitrate (`"2M"`, `"128k"`, `"500000"`) into bits/sec.
fn parse_bitrate_bps(s: &str) -> Option<f64> {
    let s = s.trim();
    if let Some(n) = s.strip_suffix(['k', 'K']) {
        n.trim().parse::<f64>().ok().map(|x| x * 1_000.0)
    } else if let Some(n) = s.strip_suffix(['m', 'M']) {
        n.trim().parse::<f64>().ok().map(|x| x * 1_000_000.0)
    } else {
        s.parse::<f64>().ok()
    }
}

#[derive(Default)]
struct Vp9Encoder {
    inner: rusty_vp9::Vp9Encoder,
}

impl Encoder for Vp9Encoder {
    fn configure(&mut self, options: &Dictionary) -> Result<()> {
        // The ffmpeg-style option names, mapped onto the native config:
        // `-qp N` sets the VP9 qindex directly (0..255); `-crf N` maps a 0..63
        // quality onto it; `-b:v RATE` (+ `-r`) engages rate control; `-lag N`
        // the ALT-REF lookahead; `-pass 2`/`twopass=1` two-pass;
        // `-cpu-used`/`-speed` the preset. Precedence and clamping live in
        // `rusty_vp9` so the semantics stay identical for native callers.
        let cfg = Vp9EncoderConfig {
            qindex: options.get("qp").and_then(|v| v.parse::<u32>().ok()),
            crf: options.get("crf").and_then(|v| v.parse::<u32>().ok()),
            q: options.get("q").and_then(|v| v.parse::<u32>().ok()),
            bitrate_bps: options.get("b").and_then(parse_bitrate_bps),
            fps: options
                .get("framerate")
                .or_else(|| options.get("r"))
                .and_then(|v| v.parse::<f64>().ok()),
            lag: options
                .get("lag")
                .or_else(|| options.get("lag-in-frames"))
                .and_then(|v| v.parse::<usize>().ok()),
            arnr_strength: options
                .get("arnr-strength")
                .or_else(|| options.get("tf"))
                .and_then(|v| v.parse::<f64>().ok()),
            dispatch_budget_ms: options
                .get("dispatch-budget")
                .and_then(|v| v.parse::<f64>().ok()),
            two_pass: options.get("pass").map(|v| v.trim()) == Some("2")
                || options.get("twopass").map(|v| v.trim()) == Some("1"),
            speed: options
                .get("cpu-used")
                .or_else(|| options.get("speed"))
                .or_else(|| options.get("quality"))
                .and_then(|v| v.parse::<u32>().ok()),
        };
        self.inner.configure(&cfg).map_err(map_err)
    }

    fn send_frame(&mut self, frame: &Frame) -> Result<()> {
        let vf: &VideoFrame = match frame {
            Frame::Video(v) => v,
            Frame::Audio(_) => {
                return Err(Error::unsupported(
                    "vp9 encode: audio frame on a video codec",
                ))
            }
        };
        if vf.format != PixelFormat::Yuv420p {
            return Err(Error::unsupported(format!(
                "vp9 encode: needs yuv420p, got `{}` (convert with -vf format=yuv420p)",
                vf.format.name()
            )));
        }
        self.inner
            .push_frame(
                [
                    vf.planes[0].as_slice(),
                    vf.planes[1].as_slice(),
                    vf.planes[2].as_slice(),
                ],
                [vf.strides[0], vf.strides[1], vf.strides[2]],
                vf.width,
                vf.height,
            )
            .map_err(map_err)
    }

    fn receive_packet(&mut self) -> Result<Packet> {
        let p = self.inner.next_packet().map_err(map_err)?;
        let mut packet = Packet::from_data(0, p.data);
        // Muxers need the random-access flag (Matroska SimpleBlock keyframe
        // bit, MP4 stss); dropping it made every seek land on a delta frame.
        packet.flags.keyframe = p.keyframe;
        packet.pts = p.pts;
        Ok(packet)
    }

    fn flush(&mut self) {
        self.inner.flush();
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Robustness fuzz: feed mutated / truncated / random byte streams (seeded
    /// from real coded frames) through the public decode API and assert the
    /// decoder never panics — a malformed stream must surface as `Err`, never a
    /// crash. `VP9_FUZZ_SEEDS` adds `.vp9` seed files; `VP9_FUZZ_ITERS` /
    /// `VP9_FUZZ_SEED` tune the run. Reproduce a crash by re-running with the
    /// printed seed.
    #[test]
    #[ignore]
    fn fuzz_robustness() {
        use std::panic::{catch_unwind, AssertUnwindSafe};
        use std::sync::Mutex;

        let mut seeds: Vec<Vec<u8>> = vec![include_bytes!("testdata/keyframe.vp9").to_vec()];
        if let Ok(dir) = std::env::var("VP9_FUZZ_SEEDS") {
            if let Ok(rd) = std::fs::read_dir(&dir) {
                for e in rd.flatten() {
                    if e.path().extension().is_some_and(|x| x == "vp9") {
                        if let Ok(d) = std::fs::read(e.path()) {
                            if !d.is_empty() {
                                seeds.push(d);
                            }
                        }
                    }
                }
            }
        }
        let iters: u64 = std::env::var("VP9_FUZZ_ITERS")
            .ok()
            .and_then(|s| s.parse().ok())
            .unwrap_or(100_000);
        let mut st: u64 = std::env::var("VP9_FUZZ_SEED")
            .ok()
            .and_then(|s| s.parse().ok())
            .unwrap_or(0x9e3779b97f4a7c15);
        let mut rng = move || {
            st ^= st << 13;
            st ^= st >> 7;
            st ^= st << 17;
            st
        };

        static LAST: Mutex<String> = Mutex::new(String::new());
        std::panic::set_hook(Box::new(|info| {
            *LAST.lock().unwrap() = info.to_string();
        }));

        let mutate = |b: &mut Vec<u8>, rng: &mut dyn FnMut() -> u64| {
            let rounds = 1 + rng() % 24;
            for _ in 0..rounds {
                if b.is_empty() {
                    b.push((rng() & 0xff) as u8);
                    continue;
                }
                match rng() % 6 {
                    0 => {
                        let i = rng() as usize % b.len();
                        b[i] ^= 1 << (rng() % 8);
                    }
                    1 => {
                        let i = rng() as usize % b.len();
                        b[i] = (rng() & 0xff) as u8;
                    }
                    2 => {
                        let n = rng() as usize % b.len();
                        b.truncate(n);
                    }
                    3 => {
                        let i = rng() as usize % (b.len() + 1);
                        b.insert(i, (rng() & 0xff) as u8);
                    }
                    4 => {
                        let i = rng() as usize % b.len();
                        b.remove(i);
                    }
                    _ => {
                        let i = rng() as usize % b.len();
                        for _ in 0..(rng() % 8) {
                            if i < b.len() {
                                b[i] = (rng() & 0xff) as u8;
                            }
                        }
                    }
                }
            }
        };

        let mut crashes = 0u64;
        for it in 0..iters {
            // Build a short packet sequence; optionally start from a clean seed so
            // inter / reference-dependent paths are reachable, then mutate.
            let npkts = 1 + rng() % 4;
            let mut packets: Vec<Vec<u8>> = Vec::new();
            for k in 0..npkts {
                if rng() % 16 == 0 {
                    packets.push(
                        (0..(rng() % 4096) as usize)
                            .map(|_| (rng() & 0xff) as u8)
                            .collect(),
                    );
                } else {
                    let mut b = seeds[rng() as usize % seeds.len()].clone();
                    if !(k == 0 && rng() % 4 == 0) {
                        mutate(&mut b, &mut rng);
                    }
                    packets.push(b);
                }
            }
            let snapshot = packets.clone();
            let res = catch_unwind(AssertUnwindSafe(|| {
                let mut dec = Vp9Decoder::default();
                for (i, pk) in packets.into_iter().enumerate() {
                    let mut p = Packet::from_data(0, pk);
                    p.pts = Some(i as i64);
                    let _ = dec.send_packet(&p);
                    for _ in 0..256 {
                        match dec.receive_frame() {
                            Ok(_) => {}
                            Err(_) => break,
                        }
                    }
                }
                dec.flush();
                for _ in 0..256 {
                    if dec.receive_frame().is_err() {
                        break;
                    }
                }
            }));
            if res.is_err() {
                crashes += 1;
                if crashes <= 12 {
                    let loc = LAST.lock().unwrap().clone();
                    let hexes: Vec<String> = snapshot
                        .iter()
                        .map(|p| {
                            p.iter()
                                .take(48)
                                .map(|b| format!("{b:02x}"))
                                .collect::<String>()
                        })
                        .collect();
                    eprintln!("[fuzz] CRASH iter={it}: {loc}\n        packets={hexes:?}");
                }
            }
        }
        let _ = std::panic::take_hook();
        assert_eq!(crashes, 0, "{crashes}/{iters} inputs crashed the decoder");
    }

    #[test]
    fn encoder_trait_roundtrips_through_registry() {
        // A 96×64 YUV420p frame through the registered encoder, then the
        // registered decoder; the decode must be valid (a key frame of the right
        // size). Bit-exactness vs the recon is covered by rusty_vp9's tests.
        let (w, h) = (96u32, 64u32);
        let ylen = (w * h) as usize;
        let clen = ((w / 2) * (h / 2)) as usize;
        let vf = VideoFrame {
            width: w,
            height: h,
            format: PixelFormat::Yuv420p,
            planes: vec![
                (0..ylen).map(|i| (i % 256) as u8).collect(),
                vec![128u8; clen],
                vec![128u8; clen],
            ],
            strides: vec![w as usize, (w / 2) as usize, (w / 2) as usize],
            pts: None,
        };

        let mut reg = rff_codec::CodecRegistry::new();
        crate::register(&mut reg);
        let mut enc = reg.find_encoder(CodecId::Vp9).unwrap();
        enc.configure(&Dictionary::new()).unwrap();
        enc.send_frame(&Frame::Video(vf)).unwrap();
        enc.flush();
        let pkt = enc.receive_packet().unwrap();
        assert!(!pkt.data.is_empty());
        // First three bytes: frame marker (10) + profile 0 + show_existing 0 +
        // key_frame bit 0 ... → byte 0 high bits 0b100... ; just confirm it decodes.

        let mut dec = reg.find_decoder(CodecId::Vp9).unwrap();
        dec.send_packet(&pkt).unwrap();
        let Frame::Video(out) = dec.receive_frame().unwrap() else {
            panic!("video")
        };
        assert_eq!((out.width, out.height), (w, h));
        assert_eq!(out.format, PixelFormat::Yuv420p);
    }

    /// ALT-REF lookahead: a `-lag N` group codes KEY + a hidden future ALT-REF + P
    /// frames + a `show_existing_frame`, and must decode to `N` displayed frames that
    /// are pixel-identical across our decoder, libvpx, and ffmpeg. Set `VP9_ARF_OUT` to
    /// dump the IVF + our decoded YUV for the external comparison.
    #[test]
    fn altref_lookahead_structure_and_roundtrip() {
        use rff_core::CodecId;
        let (w, h) = (128u32, 96u32);
        let (cw, ch) = (w as usize, h as usize);
        let n = 8u32;
        let frame = |f: u32| -> VideoFrame {
            let s = f as usize;
            let y: Vec<u8> = (0..cw * ch)
                .map(|i| (((i % cw + s) ^ (i / cw)) % 200 + 20) as u8)
                .collect();
            let uv = vec![128u8; (cw / 2) * (ch / 2)];
            VideoFrame {
                width: w,
                height: h,
                format: PixelFormat::Yuv420p,
                planes: vec![y, uv.clone(), uv],
                strides: vec![cw, cw / 2, cw / 2],
                pts: Some(f as i64),
            }
        };
        let mut reg = rff_codec::CodecRegistry::new();
        crate::register(&mut reg);
        let mut enc = reg.find_encoder(CodecId::Vp9).unwrap();
        let mut opts = Dictionary::new();
        opts.set("lag", &n.to_string());
        enc.configure(&opts).unwrap();
        for f in 0..n {
            enc.send_frame(&Frame::Video(frame(f))).unwrap();
        }
        enc.flush();
        let mut packets = Vec::new();
        while let Ok(p) = enc.receive_packet() {
            packets.push(p.data);
        }
        // One group of n frames ⇒ KEY + superframe[ARF,P1] + (n-3) P + show_existing = n.
        assert_eq!(
            packets.len() as u32,
            n,
            "expected KEY + superframe(ARF,P1) + P… + show_existing"
        );

        // Decode with our decoder; a hidden ARF yields no displayed frame, the
        // show_existing yields the ARF's frame — so exactly n frames are displayed.
        let mut dec = reg.find_decoder(CodecId::Vp9).unwrap();
        let mut ours: Vec<VideoFrame> = Vec::new();
        for pkt in &packets {
            dec.send_packet(&Packet::from_data(0, pkt.clone())).unwrap();
            while let Ok(Frame::Video(vf)) = dec.receive_frame() {
                ours.push(vf);
            }
        }
        assert_eq!(ours.len() as u32, n, "displayed frame count");

        if let Ok(dir) = std::env::var("VP9_ARF_OUT") {
            let mut ivf = Vec::new();
            ivf.extend_from_slice(b"DKIF");
            ivf.extend_from_slice(&0u16.to_le_bytes());
            ivf.extend_from_slice(&32u16.to_le_bytes());
            ivf.extend_from_slice(b"VP90");
            ivf.extend_from_slice(&(w as u16).to_le_bytes());
            ivf.extend_from_slice(&(h as u16).to_le_bytes());
            ivf.extend_from_slice(&30u32.to_le_bytes());
            ivf.extend_from_slice(&1u32.to_le_bytes());
            ivf.extend_from_slice(&(packets.len() as u32).to_le_bytes());
            ivf.extend_from_slice(&0u32.to_le_bytes());
            for (i, b) in packets.iter().enumerate() {
                ivf.extend_from_slice(&(b.len() as u32).to_le_bytes());
                ivf.extend_from_slice(&(i as u64).to_le_bytes());
                ivf.extend_from_slice(b);
            }
            std::fs::write(format!("{dir}/arf.ivf"), &ivf).unwrap();
            // Our decoded frames, display order, planar 4:2:0 (display size).
            let mut raw = Vec::new();
            for vf in &ours {
                for (p, &(pw, ph)) in [
                    (w as usize, h as usize),
                    ((w / 2) as usize, (h / 2) as usize),
                    ((w / 2) as usize, (h / 2) as usize),
                ]
                .iter()
                .enumerate()
                {
                    for yy in 0..ph {
                        raw.extend_from_slice(
                            &vf.planes[p][yy * vf.strides[p]..yy * vf.strides[p] + pw],
                        );
                    }
                }
            }
            std::fs::write(format!("{dir}/arf.ours.yuv"), &raw).unwrap();
        }
    }

    /// Two-pass rate control: on a clip whose complexity varies over time, the encode
    /// should land near the requested size (better than single-pass, which overshoots
    /// at the start before the leaky bucket catches up) and decode cleanly.
    #[test]
    fn two_pass_hits_target_and_decodes() {
        use rff_core::CodecId;
        let (w, h) = (128u32, 96u32);
        let (cw, ch) = (w as usize, h as usize);
        let n = 16u32;
        let fps = 30.0;
        // First half smooth, second half busy — a moving-complexity clip so a global
        // (lookahead) allocation clearly beats a reactive one.
        let frame = |f: u32| -> VideoFrame {
            let busy = f >= n / 2;
            let y: Vec<u8> = (0..cw * ch)
                .map(|i| {
                    let (x, yy) = (i % cw, i / cw);
                    if busy {
                        (((x * 13) ^ (yy * 7) ^ (f as usize * 5)) % 256) as u8
                    } else {
                        ((x + yy) / 3 % 200) as u8
                    }
                })
                .collect();
            let uv = vec![128u8; (cw / 2) * (ch / 2)];
            VideoFrame {
                width: w,
                height: h,
                format: PixelFormat::Yuv420p,
                planes: vec![y, uv.clone(), uv],
                strides: vec![cw, cw / 2, cw / 2],
                pts: Some(f as i64),
            }
        };

        let target = "300k";
        let mut reg = rff_codec::CodecRegistry::new();
        crate::register(&mut reg);
        let mut enc = reg.find_encoder(CodecId::Vp9).unwrap();
        let mut opts = Dictionary::new();
        opts.set("b", target);
        opts.set("twopass", "1");
        enc.configure(&opts).unwrap();
        for f in 0..n {
            enc.send_frame(&Frame::Video(frame(f))).unwrap();
        }
        enc.flush();
        let mut total_bits = 0u64;
        let mut packets = Vec::new();
        while let Ok(p) = enc.receive_packet() {
            total_bits += p.data.len() as u64 * 8;
            packets.push(p.data);
        }
        let achieved = total_bits as f64 * fps / n as f64;
        eprintln!("two-pass: target=300000 bps, achieved={achieved:.0} bps");
        // Within ±35% of target — the qindex model is coarse but the global solve keeps
        // it in the ballpark (single-pass on this clip swings far wider at the start).
        assert!(
            (achieved - 300_000.0).abs() < 0.35 * 300_000.0,
            "two-pass missed target badly: {achieved:.0} bps"
        );

        // The stream must decode to all n frames.
        let mut dec = reg.find_decoder(CodecId::Vp9).unwrap();
        let mut shown = 0u32;
        for pkt in &packets {
            dec.send_packet(&Packet::from_data(0, pkt.clone())).unwrap();
            while let Ok(Frame::Video(_)) = dec.receive_frame() {
                shown += 1;
            }
        }
        assert_eq!(shown, n, "two-pass decoded frame count");
    }

    /// ALT-REF temporal filtering: on a static scene corrupted by per-frame noise, the
    /// filter averages the motion-compensated neighbors so the ALT-REF *recovers the
    /// clean signal*. The displayed ALT-REF (last frame, via `show_existing`) is then
    /// markedly closer to the noise-free ground truth than the raw noisy anchor is —
    /// higher PSNR-vs-clean — at no cost in group size.
    #[test]
    fn temporal_filter_denoises_altref() {
        use rff_core::CodecId;
        let (w, h) = (128u32, 96u32);
        let (cw, ch) = (w as usize, h as usize);
        let n = 8u32;
        // Clean static base + strong per-frame noise (uncorrelated frame-to-frame).
        let base = |x: usize, y: usize| (((x * 5) ^ (y * 3)) % 180 + 40) as i32;
        let clean: Vec<u8> = (0..cw * ch).map(|i| base(i % cw, i / cw) as u8).collect();
        let frame = |f: u32| -> VideoFrame {
            let mut s = 0x9E3779B9u32.wrapping_mul(f + 1).wrapping_add(1);
            let mut noise = move || {
                s ^= s << 13;
                s ^= s >> 17;
                s ^= s << 5;
                (s % 41) as i32 - 20
            };
            let y: Vec<u8> = (0..cw * ch)
                .map(|i| (base(i % cw, i / cw) + noise()).clamp(0, 255) as u8)
                .collect();
            let uv = vec![128u8; (cw / 2) * (ch / 2)];
            VideoFrame {
                width: w,
                height: h,
                format: PixelFormat::Yuv420p,
                planes: vec![y, uv.clone(), uv],
                strides: vec![cw, cw / 2, cw / 2],
                pts: Some(f as i64),
            }
        };
        // Encode a group, then decode; return (group bytes, PSNR of the last displayed
        // frame — the ALT-REF — against the clean ground truth).
        let run = |strength: &str| -> (usize, f64) {
            let mut reg = rff_codec::CodecRegistry::new();
            crate::register(&mut reg);
            let mut enc = reg.find_encoder(CodecId::Vp9).unwrap();
            let mut opts = Dictionary::new();
            opts.set("lag", &n.to_string());
            opts.set("qp", "48");
            opts.set("arnr-strength", strength);
            enc.configure(&opts).unwrap();
            for f in 0..n {
                enc.send_frame(&Frame::Video(frame(f))).unwrap();
            }
            enc.flush();
            let mut total = 0;
            let mut packets = Vec::new();
            while let Ok(p) = enc.receive_packet() {
                total += p.data.len();
                packets.push(p.data);
            }
            let mut dec = reg.find_decoder(CodecId::Vp9).unwrap();
            let mut last: Option<VideoFrame> = None;
            for pkt in &packets {
                dec.send_packet(&Packet::from_data(0, pkt.clone())).unwrap();
                while let Ok(Frame::Video(vf)) = dec.receive_frame() {
                    last = Some(vf);
                }
            }
            let vf = last.unwrap();
            let mut se = 0u64;
            for y in 0..ch {
                for x in 0..cw {
                    let d = clean[y * cw + x] as i64 - vf.planes[0][y * vf.strides[0] + x] as i64;
                    se += (d * d) as u64;
                }
            }
            let mse = se as f64 / (cw * ch) as f64;
            let psnr = 10.0 * (255.0f64 * 255.0 / mse).log10();
            (total, psnr)
        };
        let (on_bytes, on_psnr) = run("4");
        let (off_bytes, off_psnr) = run("0");
        eprintln!(
            "temporal filter: ALT-REF PSNR-vs-clean off={off_psnr:.2} dB on={on_psnr:.2} dB (+{:.2}); group bytes off={off_bytes} on={on_bytes}",
            on_psnr - off_psnr
        );
        // The filtered ALT-REF recovers the clean signal far better...
        assert!(
            on_psnr > off_psnr + 2.0,
            "temporal filter did not denoise: on={on_psnr:.2} off={off_psnr:.2}"
        );
        // ...and does not cost group size.
        assert!(
            on_bytes <= off_bytes,
            "tf grew the group: on={on_bytes} off={off_bytes}"
        );
    }

    /// Cross-GOP chaining: two `-lag 8` groups over 16 frames must contain exactly ONE
    /// key frame (the very first) — the second group chains through the reference slots
    /// with no key — yet still decode to 16 displayed frames that are pixel-identical
    /// across our decoder, libvpx, and ffmpeg. `VP9_XGOP_OUT` dumps for the external arm.
    #[test]
    fn cross_gop_chaining_no_extra_keyframe() {
        use rff_core::CodecId;
        let (w, h) = (128u32, 96u32);
        let (cw, ch) = (w as usize, h as usize);
        let n = 16u32;
        let frame = |f: u32| -> VideoFrame {
            let s = f as usize;
            let y: Vec<u8> = (0..cw * ch)
                .map(|i| (((i % cw + s) ^ (i / cw + s / 2)) % 220 + 18) as u8)
                .collect();
            let uv = vec![128u8; (cw / 2) * (ch / 2)];
            VideoFrame {
                width: w,
                height: h,
                format: PixelFormat::Yuv420p,
                planes: vec![y, uv.clone(), uv],
                strides: vec![cw, cw / 2, cw / 2],
                pts: Some(f as i64),
            }
        };
        let mut reg = rff_codec::CodecRegistry::new();
        crate::register(&mut reg);
        let mut enc = reg.find_encoder(CodecId::Vp9).unwrap();
        let mut opts = Dictionary::new();
        opts.set("lag", "8");
        enc.configure(&opts).unwrap();
        for f in 0..n {
            enc.send_frame(&Frame::Video(frame(f))).unwrap();
        }
        enc.flush();
        let mut packets = Vec::new();
        while let Ok(p) = enc.receive_packet() {
            packets.push(p.data);
        }
        // A frame is a key frame iff (not show_existing and frame_type=0), i.e. the
        // show_existing (bit3) and frame_type (bit2) bits of byte0 are both 0.
        let keyframes = packets.iter().filter(|p| p[0] & 0x0C == 0).count();
        assert_eq!(
            keyframes, 1,
            "exactly one key frame expected (chained groups)"
        );

        let mut dec = reg.find_decoder(CodecId::Vp9).unwrap();
        let mut ours: Vec<VideoFrame> = Vec::new();
        for pkt in &packets {
            dec.send_packet(&Packet::from_data(0, pkt.clone())).unwrap();
            while let Ok(Frame::Video(vf)) = dec.receive_frame() {
                ours.push(vf);
            }
        }
        assert_eq!(ours.len() as u32, n, "displayed frame count");

        if let Ok(dir) = std::env::var("VP9_XGOP_OUT") {
            let mut ivf = Vec::new();
            ivf.extend_from_slice(b"DKIF");
            ivf.extend_from_slice(&0u16.to_le_bytes());
            ivf.extend_from_slice(&32u16.to_le_bytes());
            ivf.extend_from_slice(b"VP90");
            ivf.extend_from_slice(&(w as u16).to_le_bytes());
            ivf.extend_from_slice(&(h as u16).to_le_bytes());
            ivf.extend_from_slice(&30u32.to_le_bytes());
            ivf.extend_from_slice(&1u32.to_le_bytes());
            ivf.extend_from_slice(&(packets.len() as u32).to_le_bytes());
            ivf.extend_from_slice(&0u32.to_le_bytes());
            for (i, b) in packets.iter().enumerate() {
                ivf.extend_from_slice(&(b.len() as u32).to_le_bytes());
                ivf.extend_from_slice(&(i as u64).to_le_bytes());
                ivf.extend_from_slice(b);
            }
            std::fs::write(format!("{dir}/xgop.ivf"), &ivf).unwrap();
            let mut raw = Vec::new();
            for vf in &ours {
                for (p, &(pw, ph)) in [
                    (w as usize, h as usize),
                    ((w / 2) as usize, (h / 2) as usize),
                    ((w / 2) as usize, (h / 2) as usize),
                ]
                .iter()
                .enumerate()
                {
                    for yy in 0..ph {
                        raw.extend_from_slice(
                            &vf.planes[p][yy * vf.strides[p]..yy * vf.strides[p] + pw],
                        );
                    }
                }
            }
            std::fs::write(format!("{dir}/xgop.ours.yuv"), &raw).unwrap();
        }
    }

    #[test]
    fn parse_bitrate_handles_suffixes() {
        assert_eq!(parse_bitrate_bps("2M"), Some(2_000_000.0));
        assert_eq!(parse_bitrate_bps("128k"), Some(128_000.0));
        assert_eq!(parse_bitrate_bps("500000"), Some(500_000.0));
        assert_eq!(parse_bitrate_bps("oops"), None);
    }

    /// R2 — `-b:v` drives the bitrate: a higher target spends more bits, and a low
    /// target is tracked (not wildly overshot). Robust to the clip's compressibility.
    #[test]
    fn rate_control_tracks_target_bitrate() {
        let (w, h) = (96u32, 96u32);
        let (cw, ch) = (w as usize, h as usize);
        let fps = 30.0;
        let n = 12u32;

        let frame = |f: u32| -> VideoFrame {
            let shift = f as usize; // a panning texture ⇒ real inter residual
            let y: Vec<u8> = (0..cw * ch)
                .map(|i| {
                    (((i % cw + shift).wrapping_mul(31) ^ (i / cw).wrapping_mul(57)) % 256) as u8
                })
                .collect();
            let uv = vec![128u8; (cw / 2) * (ch / 2)];
            VideoFrame {
                width: w,
                height: h,
                format: PixelFormat::Yuv420p,
                planes: vec![y, uv.clone(), uv],
                strides: vec![cw, cw / 2, cw / 2],
                pts: Some(f as i64),
            }
        };

        let run = |bitrate: &str| -> f64 {
            let mut reg = rff_codec::CodecRegistry::new();
            crate::register(&mut reg);
            let mut enc = reg.find_encoder(CodecId::Vp9).unwrap();
            let mut opts = Dictionary::new();
            opts.set("b", bitrate);
            enc.configure(&opts).unwrap();
            let mut total_bits = 0u64;
            for f in 0..n {
                enc.send_frame(&Frame::Video(frame(f))).unwrap();
                while let Ok(pkt) = enc.receive_packet() {
                    total_bits += pkt.data.len() as u64 * 8;
                }
            }
            total_bits as f64 * fps / n as f64
        };

        let lo = run("120k");
        let hi = run("3M");
        eprintln!("rate control: 120k→{lo:.0} bps, 3M→{hi:.0} bps");
        // A higher target spends more bits...
        assert!(
            hi > lo * 1.5,
            "no response to target: lo={lo:.0} hi={hi:.0}"
        );
        // ...and the low target is tracked, not blown past.
        assert!(
            lo < 120_000.0 * 2.5,
            "overshot the 120k target: {lo:.0} bps"
        );
    }
}
