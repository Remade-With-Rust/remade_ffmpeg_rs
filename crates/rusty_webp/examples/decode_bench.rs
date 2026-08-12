//! Lossy (VP8) decode microbench: decode a still webp N times, print wall
//! per iteration. Build with `--features profile` for per-stage shares.
//!
//! Usage: decode_bench <file.webp> [iterations]

use std::io::Cursor;

#[global_allocator]
static GLOBAL_ALLOC: rusty_alloc_api::RustyAlloc = rusty_alloc_api::RustyAlloc;

fn main() {
    let mut args = std::env::args().skip(1);
    let path = args.next().expect("usage: decode_bench <file.webp> [iters]");
    let iters: usize = args.next().map_or(5, |s| s.parse().expect("iters"));

    let data = std::fs::read(&path).expect("read input");
    for i in 0..iters {
        let t = std::time::Instant::now();
        let mut dec = rusty_webp::WebPDecoder::new(Cursor::new(&data)).expect("parse");
        let yuv = dec
            .read_yuv420()
            .expect("decode")
            .expect("not a still lossy webp");
        let wall = t.elapsed();
        // keep the result alive / observable
        let checksum: u64 = yuv.y.iter().map(|&b| u64::from(b)).sum::<u64>()
            + yuv.u.len() as u64
            + yuv.v.len() as u64;
        println!(
            "iter {i}: {:.1}ms  ({}x{}, y_stride {}, checksum {checksum})",
            wall.as_secs_f64() * 1e3,
            yuv.width,
            yuv.height,
            yuv.y_stride,
        );
    }
}
