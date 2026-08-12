//! Per-stage profile of the single-frame GIF encode path on raw RGB24 input.
//!
//! Mirrors the exact stages of `Frame::from_rgb` + `Encoder::write_frame` so
//! each can be timed in isolation; the assembled output is asserted equal to
//! the library's own end-to-end result on every run (oracle gate).
//!
//! Usage: stage_profile <file.rgb> <width> <height> [reps]

use std::borrow::Cow;
use std::collections::BTreeSet;
use std::time::Instant;

use rusty_gif::{Encoder, Frame};

// Project convention: every encoder-carrying binary runs under rusty_alloc.
#[global_allocator]
static GLOBAL_ALLOC: rusty_alloc_api::RustyAlloc = rusty_alloc_api::RustyAlloc;

fn main() {
    let mut args = std::env::args().skip(1);
    let path = args.next().expect("usage: stage_profile <file.rgb> <w> <h> [reps]");
    let w: u16 = args.next().unwrap().parse().unwrap();
    let h: u16 = args.next().unwrap().parse().unwrap();
    let reps: usize = args.next().map(|r| r.parse().unwrap()).unwrap_or(3);

    let rgb = std::fs::read(&path).unwrap();
    assert_eq!(rgb.len(), w as usize * h as usize * 3, "bad dimensions");

    // Oracle: the library's own end-to-end encode.
    let oracle = {
        let frame = Frame::from_rgb(w, h, &rgb);
        let mut out = Vec::new();
        let mut enc = Encoder::new(&mut out, w, h, &[]).unwrap();
        enc.write_frame(&frame).unwrap();
        drop(enc);
        out
    };

    let mut best: [f64; 5] = [f64::MAX; 5];
    for _ in 0..reps {
        // Stage 1: RGB -> RGBA expand (as from_rgb_speed does).
        let t = Instant::now();
        let mut rgba: Vec<u8> = Vec::with_capacity(rgb.len() / 3 * 4);
        for v in rgb.chunks_exact(3) {
            rgba.extend_from_slice(&[v[0], v[1], v[2], 0xFF]);
        }
        let s1 = t.elapsed().as_secs_f64();

        // Stage 2: exact-palette attempt (BTreeSet scan until >256 colors).
        let t = Instant::now();
        let mut colors: BTreeSet<(u8, u8, u8, u8)> = BTreeSet::new();
        let mut exceeded = false;
        for pixel in rgba.chunks_exact(4) {
            if colors.insert((pixel[0], pixel[1], pixel[2], pixel[3])) && colors.len() > 256 {
                exceeded = true;
                break;
            }
        }
        let s2 = t.elapsed().as_secs_f64();

        let (s3, s4, frame) = if exceeded {
            // Stage 3: NeuQuant training (speed=1, as Frame::from_rgb).
            let t = Instant::now();
            let nq = rusty_gif::neuquant::NeuQuant::new(1, 256, &rgba);
            let s3 = t.elapsed().as_secs_f64();

            // Stage 4: per-pixel palette mapping.
            let t = Instant::now();
            let buffer: Vec<u8> = rgba.chunks_exact(4).map(|pix| nq.index_of(pix) as u8).collect();
            let s4 = t.elapsed().as_secs_f64();

            let frame = Frame {
                width: w,
                height: h,
                buffer: Cow::Owned(buffer),
                palette: Some(nq.color_map_rgb()),
                ..Frame::default()
            };
            (s3, s4, frame)
        } else {
            (0.0, 0.0, Frame::from_rgb(w, h, &rgb))
        };

        // Stage 5: LZW encode + container write.
        let t = Instant::now();
        let mut out = Vec::new();
        let mut enc = Encoder::new(&mut out, w, h, &[]).unwrap();
        enc.write_frame(&frame).unwrap();
        drop(enc);
        let s5 = t.elapsed().as_secs_f64();

        assert_eq!(out, oracle, "stage-assembled output diverged from library output");

        for (b, v) in best.iter_mut().zip([s1, s2, s3, s4, s5]) {
            if v < *b { *b = v; }
        }
    }

    // Content signal: full unique-color census (u32 key, dense bitset).
    let mut seen = vec![0u64; 1 << 18]; // 2^24 colors / 64
    let mut unique: u64 = 0;
    for v in rgb.chunks_exact(3) {
        let key = ((v[0] as usize) << 16) | ((v[1] as usize) << 8) | v[2] as usize;
        let (word, bit) = (key >> 6, key & 63);
        if seen[word] & (1u64 << bit) == 0 {
            seen[word] |= 1u64 << bit;
            unique += 1;
        }
    }

    let total: f64 = best.iter().sum();
    let name = std::path::Path::new(&path).file_stem().unwrap().to_string_lossy();
    println!("{name}: {unique} unique colors / {} px", rgb.len() / 3);
    println!(
        "{name} {w}x{h} total {:.1} ms  (best-of-{reps}, oracle: byte-identical)",
        total * 1e3
    );
    for (label, v) in ["rgb->rgba", "exact-scan", "nq-train", "nq-map", "lzw+write"]
        .iter()
        .zip(best)
    {
        println!("  {label:<10} {:>8.1} ms  {:>5.1}%", v * 1e3, v / total * 100.0);
    }
}
