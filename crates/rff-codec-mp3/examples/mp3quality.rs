//! NMR quality harness driver — perceptual A/B of codecs against an original.
//!
//! Usage: `mp3quality <orig.wav> <label=coded.wav> [label=coded.wav ...]`
//! Each `coded.wav` is the *decoded* output of a codec (decode both candidates with
//! the SAME neutral decoder so the comparison is fair). Prints mean/max NMR, the
//! audible %, and the per-band profile. Lower mean NMR = perceptually better.
//!
//! `cargo run -p rff-codec-mp3 --features lab --example mp3quality -- orig.wav ours=o.wav lame=l.wav`

use rff_codec_mp3::lab::quality::{track_nmr, NmrReport};
use std::{env, fs};

/// Read a RIFF/WAVE file into mono `f32` samples + its sample rate. Handles PCM s16
/// and IEEE float32; downmixes channel 0.
fn read_wav(path: &str) -> (Vec<f32>, u32) {
    let d = fs::read(path).unwrap_or_else(|e| panic!("read {path}: {e}"));
    assert!(
        d.len() > 44 && &d[0..4] == b"RIFF" && &d[8..12] == b"WAVE",
        "{path}: not a WAV"
    );
    let (mut tag, mut ch, mut sr, mut bits) = (1u16, 1u16, 44100u32, 16u16);
    let mut data: &[u8] = &[];
    let mut pos = 12;
    while pos + 8 <= d.len() {
        let id = &d[pos..pos + 4];
        let sz = u32::from_le_bytes([d[pos + 4], d[pos + 5], d[pos + 6], d[pos + 7]]) as usize;
        let body = &d[pos + 8..(pos + 8 + sz).min(d.len())];
        if id == b"fmt " && body.len() >= 16 {
            tag = u16::from_le_bytes([body[0], body[1]]);
            ch = u16::from_le_bytes([body[2], body[3]]).max(1);
            sr = u32::from_le_bytes([body[4], body[5], body[6], body[7]]);
            bits = u16::from_le_bytes([body[14], body[15]]);
        } else if id == b"data" {
            data = body;
        }
        pos += 8 + sz + (sz & 1); // chunks are word-aligned
    }
    let c = ch as usize;
    let mut out = Vec::new();
    match (tag, bits) {
        (3, 32) => {
            for fr in data.chunks_exact(4 * c) {
                out.push(f32::from_le_bytes([fr[0], fr[1], fr[2], fr[3]]));
            }
        }
        (1, 16) => {
            for fr in data.chunks_exact(2 * c) {
                out.push(i16::from_le_bytes([fr[0], fr[1]]) as f32 / 32768.0);
            }
        }
        _ => panic!("{path}: unsupported WAV (tag {tag}, {bits}-bit)"),
    }
    (out, sr)
}

fn main() {
    let args: Vec<String> = env::args().skip(1).collect();
    if args.len() < 2 {
        eprintln!("usage: mp3quality <orig.wav> <label=coded.wav> [label=coded.wav ...]");
        std::process::exit(2);
    }
    let (orig, sr) = read_wav(&args[0]);
    println!(
        "original: {}  ({:.1}s mono @ {} Hz)\n",
        args[0],
        orig.len() as f32 / sr as f32,
        sr
    );
    println!(
        "{:<8} {:>10} {:>10} {:>11} {:>7}",
        "codec", "mean NMR", "max NMR", "% audible", "delay"
    );
    println!("{}", "-".repeat(50));

    let mut reports: Vec<(String, NmrReport)> = Vec::new();
    for a in &args[1..] {
        let (label, path) = a.split_once('=').unwrap_or(("coded", a.as_str()));
        let (coded, _) = read_wav(path);
        let r = track_nmr(&orig, &coded, sr);
        println!(
            "{:<8} {:>8.1} dB {:>8.1} dB {:>9.1} % {:>7}",
            label, r.mean_nmr_db, r.max_nmr_db, r.pct_audible, r.delay
        );
        reports.push((label.to_string(), r));
    }

    // Per-band profile (mean NMR dB per band) — shows WHERE each codec is weak.
    if !reports.is_empty() {
        println!("\nper-band mean NMR (dB), low band → high:");
        for (label, r) in &reports {
            let bars: String = r
                .per_band_db
                .iter()
                .map(|&v| {
                    if v > 6.0 {
                        '#'
                    } else if v > 0.0 {
                        '+'
                    } else if v > -12.0 {
                        '.'
                    } else {
                        ' '
                    }
                })
                .collect();
            println!("  {label:<8} [{bars}]");
        }
        println!("  legend: '#' >6dB (clearly audible)  '+' 0..6dB  '.' -12..0dB  ' ' masked");
    }

    println!("\nNMR scores coding noise vs OUR psymodel's mask — compare codecs");
    println!("RELATIVELY (lower = better); the shared psymodel bias cancels.");
}
