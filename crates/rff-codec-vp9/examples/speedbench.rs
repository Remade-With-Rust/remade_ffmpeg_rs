//! VP9 encoder speed/size bench that bypasses the CLI (and its opus dependency).
//!
//! Reads a `.y4m` (4:2:0 only), encodes the whole clip through the registered VP9
//! encoder at a given `-crf` and `-cpu-used` (speed preset), times the encode loop,
//! and writes an `.ivf` so an external tool can PSNR it.
//!
//! Usage: `cargo run --release -p rff-codec-vp9 --example speedbench -- <in.y4m> <out.ivf> <crf> <speed>`

use std::io::Write;
use std::time::Instant;

use rff_codec::CodecRegistry;
use rff_core::{CodecId, Dictionary, Frame, VideoFrame};

fn main() {
    let a: Vec<String> = std::env::args().collect();
    let (path, out, crf, speed) = (&a[1], &a[2], &a[3], &a[4]);
    let lag = a.get(5).cloned();

    // --- minimal y4m parse (4:2:0, 8-bit) ---
    let buf = std::fs::read(path).unwrap();
    let nl = buf.iter().position(|&b| b == b'\n').unwrap();
    let header = std::str::from_utf8(&buf[..nl]).unwrap();
    let (mut w, mut h) = (0usize, 0usize);
    for tok in header.split_whitespace() {
        match tok.as_bytes().first() {
            Some(b'W') => w = tok[1..].parse().unwrap(),
            Some(b'H') => h = tok[1..].parse().unwrap(),
            _ => {}
        }
    }
    let (cw, ch) = (w.div_ceil(2), h.div_ceil(2));
    let fsize = w * h + 2 * cw * ch;
    let mut frames: Vec<VideoFrame> = Vec::new();
    let mut pos = nl + 1;
    while pos < buf.len() {
        // Each frame is `FRAME...\n` then packed Y, U, V.
        let fnl = buf[pos..].iter().position(|&b| b == b'\n').map(|i| pos + i);
        let Some(fnl) = fnl else { break };
        let start = fnl + 1;
        if start + fsize > buf.len() {
            break;
        }
        let y = buf[start..start + w * h].to_vec();
        let u = buf[start + w * h..start + w * h + cw * ch].to_vec();
        let v = buf[start + w * h + cw * ch..start + fsize].to_vec();
        frames.push(VideoFrame {
            width: w as u32,
            height: h as u32,
            format: rff_core::PixelFormat::Yuv420p,
            planes: vec![y, u, v],
            strides: vec![w, cw, cw],
            pts: None,
        });
        pos = start + fsize;
    }

    // --- encoder ---
    let mut reg = CodecRegistry::new();
    rff_codec_vp9::register(&mut reg);
    let mut enc = reg.find_encoder(CodecId::Vp9).unwrap();
    let mut opts = Dictionary::new();
    opts.set("crf", crf);
    opts.set("cpu-used", speed);
    // Optional 5th arg: lag-in-frames (ALT-REF lookahead). libvpx defaults to 25;
    // ours defaults to 0, so this makes the two comparable.
    if let Some(l) = &lag {
        opts.set("lag", l);
    }
    enc.configure(&opts).unwrap();

    let mut packets: Vec<Vec<u8>> = Vec::new();
    let t0 = Instant::now();
    for f in &frames {
        enc.send_frame(&Frame::Video(f.clone())).unwrap();
        while let Ok(p) = enc.receive_packet() {
            packets.push(p.data);
        }
    }
    enc.flush();
    while let Ok(p) = enc.receive_packet() {
        packets.push(p.data);
    }
    let dt = t0.elapsed().as_secs_f64();

    // --- write IVF ---
    // Observe-only: which reference did the emitted inter blocks actually pick?
    if std::env::var("VP9_REF_HIST").is_ok() {
        let h = rff_codec_vp9::ref_hist_take();
        let n: u64 = h.iter().sum();
        let pct = |v: u64| {
            if n > 0 {
                100.0 * v as f64 / n as f64
            } else {
                0.0
            }
        };
        eprintln!(
            "REF_HIST blocks={n}  LAST {:.1}%  GOLDEN {:.1}%  ALTREF {:.1}%  COMPOUND {:.1}%",
            pct(h[0]),
            pct(h[1]),
            pct(h[2]),
            pct(h[3])
        );
    }
    let total: usize = packets.iter().map(|p| p.len()).sum();
    write_ivf(out, w as u16, h as u16, &packets);
    eprintln!(
        "speed={speed} crf={crf}: {n} frames in {dt:.2}s = {fps:.1} fps  |  {total} B",
        n = frames.len(),
        fps = frames.len() as f64 / dt,
    );
}

fn write_ivf(path: &str, w: u16, h: u16, packets: &[Vec<u8>]) {
    let mut f = std::io::BufWriter::new(std::fs::File::create(path).unwrap());
    let mut hdr = Vec::new();
    hdr.extend_from_slice(b"DKIF");
    hdr.extend_from_slice(&0u16.to_le_bytes()); // version
    hdr.extend_from_slice(&32u16.to_le_bytes()); // header len
    hdr.extend_from_slice(b"VP90");
    hdr.extend_from_slice(&w.to_le_bytes());
    hdr.extend_from_slice(&h.to_le_bytes());
    hdr.extend_from_slice(&30u32.to_le_bytes()); // fps num
    hdr.extend_from_slice(&1u32.to_le_bytes()); // fps den
    hdr.extend_from_slice(&(packets.len() as u32).to_le_bytes());
    hdr.extend_from_slice(&0u32.to_le_bytes()); // unused
    f.write_all(&hdr).unwrap();
    for (i, p) in packets.iter().enumerate() {
        f.write_all(&(p.len() as u32).to_le_bytes()).unwrap();
        f.write_all(&(i as u64).to_le_bytes()).unwrap(); // pts
        f.write_all(p).unwrap();
    }
}
