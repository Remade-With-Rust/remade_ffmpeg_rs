//! Decode one JPEG N times. A clean, single-purpose executable so a pinned,
//! CPU-time measurement has something to measure that is not cargo, not a test
//! harness, and not file I/O.
//!
//! ```text
//! decode_bench <file.jpg> [reps] [planar|rgb]
//! ```

use rusty_jpeg::decode::Decoder;
use std::io::Cursor;

fn main() {
    let mut args = std::env::args().skip(1);
    let path = args
        .next()
        .expect("usage: decode_bench <file.jpg> [reps] [mode]");
    let reps: usize = args.next().and_then(|v| v.parse().ok()).unwrap_or(100);
    let mode = args.next().unwrap_or_else(|| "planar".into());
    // 4th arg: "st" forces single-threaded decoding.
    let single = args.next().map(|v| v == "st").unwrap_or(false);

    // Read once, up front: this benchmarks the codec, not the filesystem.
    let bytes = std::fs::read(&path).expect("read jpeg");

    let verify = std::env::var("RUSTY_JPEG_VERIFY").is_ok();
    let mut sink = 0u64;
    // Recycle output planes between frames, as any streaming consumer would.
    // Set RUSTY_JPEG_ABLATE=nopool to measure without it.
    let pool_off = std::env::var("RUSTY_JPEG_ABLATE")
        .map(|v| v.split(',').any(|t| t == "nopool"))
        .unwrap_or(false);
    let mut pool: Vec<Vec<u8>> = Vec::new();
    for _ in 0..reps {
        let mut d = Decoder::new(Cursor::new(&bytes));
        d.set_single_threaded(single);
        if !pool_off {
            d.recycle_planes(std::mem::take(&mut pool));
        }
        if mode == "headers" {
            // Prices per-frame SETUP only: construct the decoder, parse the
            // markers, build the Huffman LUTs. Everything the full decode pays
            // before a single coefficient is read.
            d.read_info().expect("read_info");
            let info = d.info().expect("info");
            sink = sink.wrapping_add(info.width as u64);
        } else if mode == "rgb" {
            let px = d.decode().expect("decode");
            sink = sink
                .wrapping_add(px.len() as u64)
                .wrapping_add(px[0] as u64);
        } else {
            let img = d.decode_planar().expect("decode_planar");
            // FULL-CONTENT hash under `RUSTY_JPEG_VERIFY=1` only. `len + data[0]`
            // is too weak to validate buffer recycling — a recycled plane keeps
            // the previous frame's pixels, so an unwritten byte would slip past
            // a sampled checksum. But hashing walks 3.1 MB per 1080p decode, so
            // leaving it on would put that work inside the timed arm and make
            // every comparison against an external reference meaningless.
            if verify {
                for c in &img.components {
                    for &b in &c.data {
                        sink ^= b as u64;
                        sink = sink.wrapping_mul(0x100000001b3);
                    }
                }
            } else {
                sink = sink
                    .wrapping_add(img.components[0].data.len() as u64)
                    .wrapping_add(img.components[0].data[0] as u64);
            }
            if !pool_off {
                pool = img.into_planes();
            }
        }
    }
    // Consume the result so nothing above can be optimized away.
    println!("{reps} decodes, checksum {sink}");
    println!("{}", rusty_jpeg::prof::dump());
    let c = rusty_jpeg::prof::read();
    use rusty_jpeg::prof::Count;
    let blocks = c[Count::DecBlocks as usize];
    let reps_f = reps as f64;
    for (name, idx) in [
        ("refills", Count::DecRefills as usize),
        ("bytes_read", Count::DecBytesRead as usize),
        ("symbols", Count::DecSymbols as usize),
        ("receive_extend", Count::DecReceiveExtend as usize),
        ("lut_hit", Count::DecLutHit as usize),
        ("lut_MISS", Count::DecLutMiss as usize),
        ("fast_ac_hit", Count::DecFastAcHit as usize),
        ("fast_ac_miss", Count::DecFastAcMiss as usize),
        ("idct_PAIRS", Count::DecIdctPairs as usize),
    ] {
        if c[idx] > 0 {
            println!(
                "  {name:<16} {:>12} total  {:>10.0} /frame",
                c[idx],
                c[idx] as f64 / reps_f
            );
        }
    }
    if blocks > 0 {
        println!(
            "dec_blocks {blocks}, dc_only {} = {:.1}%",
            c[Count::DecDcOnlyBlocks as usize],
            100.0 * c[Count::DecDcOnlyBlocks as usize] as f64 / blocks as f64
        );
    }
}

// Primary allocator for this target: our rusty_alloc, the pure-Rust mimalloc
// remake. Codec hot paths allocate heavily and the system heap dominates the
// profile there (measured 1.38x end-to-end on AV2 decode). Per project
// convention this belongs in binary/bench/example roots, never in a library --
// a library that declares one hijacks every dependent's allocator choice.
#[global_allocator]
static RUSTY_ALLOC: rusty_alloc_api::RustyAlloc = rusty_alloc_api::RustyAlloc;
