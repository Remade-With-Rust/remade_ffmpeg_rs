//! Decode-speed bench: `aacbench <file.aac (ADTS)> [runs]` — best-of-N wall time
//! for decoding the whole stream, reported as ms and ×realtime.

use rusty_aac::{parse_adts, AacDecoder};
use std::time::Instant;

#[global_allocator]
static RUSTY_ALLOC: rusty_alloc_api::RustyAlloc = rusty_alloc_api::RustyAlloc;

fn main() {
    let args: Vec<String> = std::env::args().collect();
    let path = args.get(1).expect("usage: aacbench <file.aac> [runs]");
    let runs: usize = args.get(2).and_then(|s| s.parse().ok()).unwrap_or(7);
    let data = std::fs::read(path).expect("read input");
    let mut frames = Vec::new();
    let mut pos = 0;
    while pos + 7 <= data.len() {
        let Ok(h) = parse_adts(&data[pos..]) else {
            break;
        };
        let len = h.frame_length;
        if len == 0 || pos + len > data.len() {
            break;
        }
        frames.push(&data[pos..pos + len]);
        pos += len;
    }
    let mut best = f64::MAX;
    let (mut samples, mut rate, mut channels) = (0usize, 0u32, 0u16);
    for _ in 0..runs {
        let mut dec = AacDecoder::new();
        let t = Instant::now();
        samples = 0;
        for f in &frames {
            if let Ok(a) = dec.decode(f, None) {
                samples += a.frames();
                rate = a.sample_rate;
                channels = a.channels;
            }
        }
        best = best.min(t.elapsed().as_secs_f64());
    }
    let secs = samples as f64 / f64::from(rate.max(1));
    println!(
        "{path}: {} frames, {samples} samples @ {rate} Hz x{channels}: best {:.2} ms ({:.1}x realtime)",
        frames.len(),
        best * 1e3,
        secs / best
    );
}
