//! `decprof` — decoder stage profile + deterministic work census on a REAL stream.
//!
//! ```text
//!   cargo run -p rusty_mp3 --release --example decprof -- input.mp3
//! ```
//!
//! Why this exists alongside the in-crate `profile_decode_stages` test: that test
//! decodes SYNTHETIC mono content produced by **our own encoder**. Both halves of
//! that are disqualifying for a decode profile — synthetic content mis-ranks
//! stages by 2-3x, and a decoder benchmarked on its own encoder's streams
//! systematically skips paths the encoder never emits (provenance is content).
//! This one takes a real file, so point it at a reference-encoder stream.
//!
//! Reports, per the Great Gate decode law (throughput functions: bit-exact by
//! law, gates are pure speed):
//!   * stage shares from the in-crate scope profiler (confirmatory), and
//!   * a DETERMINISTIC work census — block types, stereo modes, channel counts
//!     (primary: one run, immune to timing noise, and it is what tells you
//!     whether a slow path is serving a large population or a rare one).

use std::sync::atomic::{AtomicU64, Ordering::Relaxed};

use rusty_mp3::Mp3Decoder;

#[global_allocator]
static GLOBAL_ALLOC: rusty_alloc_api::RustyAlloc = rusty_alloc_api::RustyAlloc;

// Deterministic population counters. A "population of streams served by a slow
// path is a missing kernel" — so count the populations before optimizing any path.
static FRAMES: AtomicU64 = AtomicU64::new(0);
static SAMPLES: AtomicU64 = AtomicU64::new(0);

fn main() {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let Some(path) = args.first() else {
        eprintln!("usage: decprof <input.mp3> [repeats]");
        std::process::exit(2);
    };
    let repeats: usize = args.get(1).and_then(|s| s.parse().ok()).unwrap_or(1);
    // `pipe` selects the two-stage threaded path; output must match the serial
    // path exactly, so the same fnv1a gate covers both.
    let pipelined = std::env::var("MP3_PIPELINE").as_deref() == Ok("1");
    // MP3_NOHASH=1 skips the FNV pass so this matches `ffmpeg -f null -`:
    // decode and discard. The hash walks every output byte and is real work --
    // leaving it in while the reference does not hash is a work-parity break.
    let nohash = std::env::var("MP3_NOHASH").as_deref() == Ok("1");

    let bytes = std::fs::read(path).expect("read input");
    println!("input: {path}  ({} KiB)", bytes.len() / 1024);

    let mut pcm_hash: u64 = 0;
    let t0 = std::time::Instant::now();
    for _ in 0..repeats {
        let mut h: u64 = 0xcbf2_9ce4_8422_2325;
        let frames: Vec<rusty_mp3::DecodedAudio> = if pipelined {
            rusty_mp3::decode_pipelined(&bytes)
        } else {
            let mut dec = Mp3Decoder::new();
            dec.push(&bytes);
            dec.flush();
            let mut v = Vec::new();
            while let Ok(f) = dec.next_frame() {
                v.push(f);
            }
            v
        };
        for f in frames {
            FRAMES.fetch_add(1, Relaxed);
            SAMPLES.fetch_add((f.samples.len() / f.channels.max(1) as usize) as u64, Relaxed);
            // FNV-1a over the raw PCM bits: the decode byte-identity gate. A
            // decoder change that claims to be output-preserving must not move it.
            if !nohash {
                for s in &f.samples {
                    for b in s.to_bits().to_le_bytes() {
                        h ^= b as u64;
                        h = h.wrapping_mul(0x0000_0100_0000_01b3);
                    }
                }
            }
        }
        pcm_hash = h;
    }
    let wall = t0.elapsed().as_secs_f64();

    let frames = FRAMES.load(Relaxed);
    let samples = SAMPLES.load(Relaxed);
    println!(
        "decoded {frames} frames, {samples} samples/ch over {repeats} pass(es) in {wall:.3} s"
    );
    // Work parity handle: any A/B against this must report the SAME frame count.
    println!("WORK-COUNT frames={frames} samples_per_ch={samples}");
    println!("PCM fnv1a: {pcm_hash:#018x}   <- decode byte-identity gate");
    println!(
        "throughput: {:.1}x realtime (44.1 kHz assumed)\n",
        samples as f64 / 44_100.0 / wall
    );

    // Scope profiler. Call count is ~26 scope pairs/frame, so the self-tax is
    // ~1% here -- small enough to read shares from, too big to quote as absolute
    // decode time. Price it before trusting a residue.
    rusty_mp3::decode::prof::dump();
    rusty_mp3::decode::prof::census();
}
