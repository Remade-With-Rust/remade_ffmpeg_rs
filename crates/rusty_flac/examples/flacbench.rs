//! In-process encode/decode bench + wiring-audit stats over a raw PCM file.
//!
//! Usage:
//!   flacbench <raw-file> <channels> <bps:16|24> <level> [reps] [--decode]
//!
//! The raw file is interleaved little-endian s16 (bps=16) or s24 (bps=24),
//! e.g. produced by `ffmpeg -i in.wav -f s16le out.raw`. Prints encode (or
//! decode) CPU-adjacent wall per rep (pin the process externally for real
//! numbers), the output size, and the EncodeStats counters.

use std::time::Instant;

// Project convention: encoder benches run under rusty_alloc (what ships).
#[global_allocator]
static GLOBAL_ALLOC: rusty_alloc_api::RustyAlloc = rusty_alloc_api::RustyAlloc;

fn main() {
    let args: Vec<String> = std::env::args().collect();
    if args.len() < 5 {
        eprintln!("usage: flacbench <raw> <channels> <bps> <level> [reps] [--decode]");
        std::process::exit(2);
    }
    let raw = std::fs::read(&args[1]).expect("read raw input");
    let channels: u32 = args[2].parse().unwrap();
    let bps: u32 = args[3].parse().unwrap();
    let level: u32 = args[4].parse().unwrap();
    let reps: u32 = args.get(5).and_then(|s| s.parse().ok()).unwrap_or(1);
    let do_decode = args.iter().any(|a| a == "--decode");

    // Unpack interleaved samples.
    let bytes_per = (bps / 8) as usize;
    let n = raw.len() / bytes_per;
    let mut samples: Vec<i32> = Vec::with_capacity(n);
    match bps {
        16 => {
            for c in raw.chunks_exact(2) {
                samples.push(i16::from_le_bytes([c[0], c[1]]) as i32);
            }
        }
        24 => {
            for c in raw.chunks_exact(3) {
                samples.push(i32::from_le_bytes([0, c[0], c[1], c[2]]) >> 8);
            }
        }
        _ => panic!("bps must be 16 or 24"),
    }

    // One stats pass (deterministic), then timed reps.
    let encode_once = |with_stats: bool| -> (Vec<u8>, Option<rusty_flac::EncodeStats>) {
        let mut enc = rusty_flac::Encoder::new(44100, channels, bps).expect("encoder");
        enc.set_compression_level(level);
        enc.push_interleaved(&samples).expect("push");
        if with_stats {
            let (out, stats) = enc.finish_with_stats();
            (out, Some(stats))
        } else {
            (enc.finish(), None)
        }
    };

    let (stream, stats) = encode_once(true);
    println!(
        "output: {} bytes ({} samples x {} ch)",
        stream.len(),
        n / channels as usize,
        channels
    );
    println!("stats: {:#?}", stats.unwrap());

    if do_decode {
        // Verify once, then time decode reps.
        let (info, chans) = rusty_flac::decode(&stream).expect("decode");
        assert_eq!(info.channels, channels);
        let total: usize = chans.iter().map(|c| c.len()).sum();
        assert_eq!(total, n, "decode sample count mismatch");
        let mut best = f64::INFINITY;
        for _ in 0..reps {
            let t = Instant::now();
            let (_, chans) = rusty_flac::decode(&stream).expect("decode");
            let dt = t.elapsed().as_secs_f64();
            std::hint::black_box(&chans);
            best = best.min(dt);
        }
        println!("decode best-of-{reps}: {:.1} ms", best * 1e3);
    } else {
        let mut best = f64::INFINITY;
        for _ in 0..reps {
            let t = Instant::now();
            let (out, _) = encode_once(false);
            let dt = t.elapsed().as_secs_f64();
            std::hint::black_box(&out);
            best = best.min(dt);
        }
        println!("encode best-of-{reps}: {:.1} ms", best * 1e3);
    }
}
