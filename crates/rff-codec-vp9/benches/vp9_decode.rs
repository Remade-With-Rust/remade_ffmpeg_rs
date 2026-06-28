//! VP9 decode throughput benchmark (dependency-free; run with
//! `cargo bench -p rff-codec-vp9`).
//!
//! Decodes a committed 1280×720 4:2:0 VP9 clip many times and reports
//! frames/sec and Mpixels/sec — the in-house decoder's headline number, on the
//! same content FFmpeg/libvpx can be timed against (see `docs/benchmarks.md`).
//! Wall-clock, single-thread, median of repeated passes after warm-up.

use std::time::Instant;

use rff_codec::CodecRegistry;
use rff_core::{CodecId, Error, Packet};

/// Split an IVF file into its raw VP9 frames + frame size.
fn parse_ivf(data: &[u8]) -> (Vec<Vec<u8>>, u32, u32) {
    assert!(
        data.len() >= 32 && &data[0..4] == b"DKIF",
        "not an IVF file"
    );
    let header_len = u16::from_le_bytes([data[6], data[7]]) as usize;
    let width = u16::from_le_bytes([data[12], data[13]]) as u32;
    let height = u16::from_le_bytes([data[14], data[15]]) as u32;
    let mut frames = Vec::new();
    let mut pos = header_len;
    while pos + 12 <= data.len() {
        let size =
            u32::from_le_bytes([data[pos], data[pos + 1], data[pos + 2], data[pos + 3]]) as usize;
        pos += 12; // 4-byte size + 8-byte timestamp
        if pos + size > data.len() {
            break;
        }
        frames.push(data[pos..pos + size].to_vec());
        pos += size;
    }
    (frames, width, height)
}

/// Decode every packet through a fresh decoder; return the frame count.
fn decode_all(registry: &CodecRegistry, packets: &[Packet]) -> u64 {
    let mut decoder = registry.find_decoder(CodecId::Vp9).expect("vp9 decoder");
    let mut frames = 0u64;
    for pkt in packets {
        let _ = decoder.send_packet(pkt);
        loop {
            match decoder.receive_frame() {
                Ok(_) => frames += 1,
                Err(Error::Again) | Err(Error::Eof) => break,
                Err(e) => {
                    eprintln!("decode error: {e}");
                    break;
                }
            }
        }
    }
    frames
}

fn main() {
    // Default: the committed 720p clip; override with VP9_BENCH_CLIP=<file.ivf>.
    let owned = std::env::var("VP9_BENCH_CLIP")
        .ok()
        .map(|p| std::fs::read(p).unwrap());
    let ivf: &[u8] = owned
        .as_deref()
        .unwrap_or(include_bytes!("data/vp9_720p.ivf"));
    let (raw_frames, w, h) = parse_ivf(ivf);
    let packets: Vec<Packet> = raw_frames
        .iter()
        .map(|f| Packet::from_data(0, f.clone()))
        .collect();

    let mut registry = CodecRegistry::new();
    rff_codec_vp9::register(&mut registry);

    let decoded = decode_all(&registry, &packets);
    assert!(decoded > 0, "no frames decoded — VP9 input/decoder problem");
    eprintln!(
        "VP9 decode benchmark: {w}x{h}, {} packets, {decoded} frames/pass",
        packets.len()
    );

    // Warm up (let the branch predictor / caches settle).
    for _ in 0..2 {
        decode_all(&registry, &packets);
    }

    let passes = 50usize;
    let mut times = Vec::with_capacity(passes);
    for _ in 0..passes {
        let start = Instant::now();
        let n = decode_all(&registry, &packets);
        times.push(start.elapsed());
        std::hint::black_box(n);
    }
    times.sort();
    let median = times[passes / 2];
    let best = times[0];

    let secs = median.as_secs_f64();
    let fps = decoded as f64 / secs;
    let mpix = (decoded * w as u64 * h as u64) as f64 / secs / 1e6;

    println!("--- VP9 decode ({w}x{h}, 4:2:0) ---");
    println!("frames/pass : {decoded}");
    println!(
        "pass time   : median {:.2} ms,  best {:.2} ms  ({passes} passes)",
        secs * 1000.0,
        best.as_secs_f64() * 1000.0
    );
    println!("throughput  : {fps:.0} fps,  {mpix:.1} Mpixels/s  (single-thread)");
}
