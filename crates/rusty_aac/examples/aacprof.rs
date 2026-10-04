//! Stage profile + SIMD-arm census over real workloads, through the SHIPPING
//! entry points (`AacEncoder`, `AacDecoder`).
//!
//! ```text
//! cargo run -p rusty_aac --release --features profile,lab --example aacprof -- enc a.wav b.wav ...
//! cargo run -p rusty_aac --release --features profile,lab --example aacprof -- dec a.aac b.aac ...
//! ```
//!
//! Stage times are CPU time summed over the encoder's worker threads; read the
//! shares. Census lines are deterministic element counts per kernel arm.

use rusty_aac::{parse_adts, prof, AacDecoder, AacEncoder, AacEncoderConfig};

#[global_allocator]
static RUSTY_ALLOC: rusty_alloc_api::RustyAlloc = rusty_alloc_api::RustyAlloc;

fn main() {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let (mode, files) = args.split_first().expect("usage: aacprof enc|dec <files...>");
    let _ = prof::take();
    let t = std::time::Instant::now();
    match mode.as_str() {
        "enc" => {
            for f in files {
                let w = rusty_aac::lab::wav::read(f).expect("read wav");
                let mut enc = AacEncoder::new(AacEncoderConfig::default());
                enc.push_pcm(&w.samples, w.channels, w.sample_rate).expect("push");
                enc.finish();
                while enc.next_packet().is_ok() {}
            }
        }
        "dec" => {
            for f in files {
                let data = std::fs::read(f).expect("read aac");
                let mut dec = AacDecoder::new();
                let mut pos = 0;
                while pos + 7 <= data.len() {
                    let Ok(h) = parse_adts(&data[pos..]) else { break };
                    let len = h.frame_length;
                    if len == 0 || pos + len > data.len() {
                        break;
                    }
                    let _ = dec.decode(&data[pos..pos + len], None);
                    pos += len;
                }
            }
        }
        _ => panic!("mode must be enc or dec"),
    }
    let wall = t.elapsed().as_secs_f64() * 1e3;
    let (stages, kernels) = prof::take();
    let total: u64 = stages.iter().map(|s| s.1).sum();
    println!("# {mode}: wall {wall:.1} ms, instrumented stage CPU {:.1} ms", total as f64 / 1e6);
    for (name, ns, calls) in stages.iter().filter(|s| s.2 > 0) {
        println!(
            "  {name:34} {:9.2} ms  {:5.1}%  {calls:>9} calls",
            *ns as f64 / 1e6,
            100.0 * *ns as f64 / total.max(1) as f64
        );
    }
    for (name, simd, scalar) in kernels.iter().filter(|k| k.1 + k.2 > 0) {
        println!(
            "  census {name:14} simd {simd:>12}  scalar {scalar:>10}  -> {:.2}% of elements reach the SIMD twin",
            100.0 * *simd as f64 / (*simd + *scalar).max(1) as f64
        );
    }
}
