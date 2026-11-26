//! Decode a HEIF file: dump the primary image, or time whole-file decodes.
//!
//! ```text
//! heif_bench dump  <file> <out.raw>
//! heif_bench bench <file> <iters> <threads>
//! ```
//!
//! `dump` writes `HEIFRAW w h chroma bits alpha premultiplied alpha_bits\n`
//! then the Y, Cb, Cr and alpha planes (bytes at 8 bits, little-endian u16
//! above). `bench` decodes the file `iters` times from memory -- parse, decode,
//! compose, transform, pack to planar bytes -- and prints each iteration's
//! nanoseconds (`threads`: 0 = all cores, 1 = calling thread only).

#[global_allocator]
static GLOBAL_ALLOC: rusty_alloc_api::RustyAlloc = rusty_alloc_api::RustyAlloc;

use rff_format_heif::{Chroma, DecodeOptions, Heif};
use std::time::Instant;

fn main() {
    let a: Vec<String> = std::env::args().collect();
    match a.get(1).map(String::as_str) {
        Some("dump") => {
            let data = std::fs::read(&a[2]).expect("read input");
            let heif = match Heif::parse(&data) {
                Ok(h) => h,
                Err(e) => fail(e),
            };
            let d = match heif.decode_primary() {
                Ok(d) => d,
                Err(e) => fail(e),
            };
            let cf = match d.image.chroma {
                Chroma::Monochrome => 0,
                Chroma::Yuv420 => 1,
                Chroma::Yuv422 => 2,
                Chroma::Yuv444 => 3,
            };
            let (has_a, premul, abd) = d
                .alpha
                .as_ref()
                .map_or((0, 0, 0), |a| (1, a.premultiplied as u8, a.bit_depth));
            let mut out = format!(
                "HEIFRAW {} {} {cf} {} {has_a} {premul} {abd}\n",
                d.image.width, d.image.height, d.image.bit_depth
            )
            .into_bytes();
            out.extend_from_slice(&d.image.to_planar_bytes());
            if let Some(al) = &d.alpha {
                for &v in &al.plane.data {
                    if al.bit_depth > 8 {
                        out.extend_from_slice(&v.to_le_bytes());
                    } else {
                        out.push(v as u8);
                    }
                }
            }
            std::fs::write(&a[3], out).expect("write output");
        }
        Some("bench") => {
            let data = std::fs::read(&a[2]).expect("read input");
            let iters: usize = a[3].parse().expect("iters");
            let threads: usize = a[4].parse().expect("threads");
            let opts = DecodeOptions {
                threads,
                ..DecodeOptions::default()
            };
            let mut px = 0;
            let mut bytes = 0;
            for _ in 0..iters {
                let t = Instant::now();
                let heif = Heif::parse_with(&data, opts).expect("parse");
                let d = heif.decode_primary().expect("decode");
                let planar = d.image.to_planar_bytes();
                bytes = planar.len();
                px = d.image.width as usize * d.image.height as usize;
                println!("ns {}", t.elapsed().as_nanos());
            }
            println!("px {px} bytes {bytes}");
        }
        _ => eprintln!("usage: heif_bench dump <file> <out> | bench <file> <iters> <threads>"),
    }
}

fn fail(e: rff_core::Error) -> ! {
    eprintln!("ERROR {e}");
    std::process::exit(2)
}
