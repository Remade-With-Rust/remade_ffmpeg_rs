//! `encprof` — encoder stage profile + noise-shaping census on a REAL file.
//!
//! ```text
//!   cargo run -p rusty_mp3 --release --example encprof -- input.wav 192
//! ```
//!
//! The census is the point. The distortion loop's whole job is per-band noise
//! shaping — the thing LAME does and we are ~0.7-1.1 ODG behind on — and a code
//! comment claims it keeps iteration 0 in every granule, i.e. never shapes at
//! all. That is a deterministic, countable property, so count it on real music
//! instead of believing the comment.

use rusty_mp3::encode::prof;
use rusty_mp3::{Mp3Encoder, Mp3EncoderConfig};

#[global_allocator]
static GLOBAL_ALLOC: rusty_alloc_api::RustyAlloc = rusty_alloc_api::RustyAlloc;

/// Minimal WAV reader: returns interleaved f32 plus (rate, channels).
fn read_wav(path: &str) -> (Vec<f32>, u32, u16) {
    let d = std::fs::read(path).expect("read wav");
    let rate = u32::from_le_bytes([d[24], d[25], d[26], d[27]]);
    let ch = u16::from_le_bytes([d[22], d[23]]);
    let bits = u16::from_le_bytes([d[34], d[35]]);
    let mut i = 12;
    while i + 8 <= d.len() {
        let id = &d[i..i + 4];
        let sz = u32::from_le_bytes([d[i + 4], d[i + 5], d[i + 6], d[i + 7]]) as usize;
        if id == b"data" {
            let body = &d[i + 8..(i + 8 + sz).min(d.len())];
            let pcm = if bits == 16 {
                body.chunks_exact(2)
                    .map(|b| i16::from_le_bytes([b[0], b[1]]) as f32 / 32768.0)
                    .collect()
            } else {
                body.chunks_exact(4)
                    .map(|b| f32::from_le_bytes([b[0], b[1], b[2], b[3]]))
                    .collect()
            };
            return (pcm, rate, ch);
        }
        i += 8 + sz + (sz & 1);
    }
    panic!("no data chunk");
}

fn main() {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let Some(path) = args.first() else {
        eprintln!("usage: encprof <input.wav> [kbps]");
        std::process::exit(2);
    };
    let kbps: u32 = args.get(1).and_then(|s| s.parse().ok()).unwrap_or(192);
    let (pcm, rate, ch) = read_wav(path);

    let mut enc = Mp3Encoder::new(Mp3EncoderConfig {
        bitrate_kbps: kbps,
        vbr_quality: None,
    });
    enc.push_pcm_f32(&pcm, ch, rate).unwrap();
    enc.finish();
    let mut bytes = 0usize;
    while let Ok(p) = enc.next_packet() {
        bytes += p.len();
    }
    let secs = pcm.len() as f64 / ch.max(1) as f64 / rate as f64;
    println!(
        "{path}: {ch} ch @ {rate} Hz, {secs:.1} s -> {bytes} bytes ({:.1} kbps)",
        bytes as f64 * 8.0 / secs / 1000.0
    );

    use std::sync::atomic::Ordering::Relaxed;
    let (nl, ns) = (prof::N_LONG.load(Relaxed), prof::N_SHORT.load(Relaxed));
    let (k0, ot) = (
        prof::OUTER_KEPT0.load(Relaxed),
        prof::OUTER_TOTAL.load(Relaxed),
    );
    println!(
        "  blocks: {nl} long, {ns} short ({:.1}% short)",
        100.0 * ns as f64 / (nl + ns).max(1) as f64
    );
    println!(
        "  NOISE SHAPING: distortion loop kept iteration 0 in {k0}/{ot} long granules ({:.1}%)",
        100.0 * k0 as f64 / ot.max(1) as f64
    );
    println!(
        "  => shaping is {}",
        if ot > 0 && k0 * 100 / ot.max(1) >= 95 {
            "EFFECTIVELY INERT (we ship one global gain per granule)"
        } else {
            "ACTIVE"
        }
    );
    prof::dump();
}
