//! Decode files: `jpxdec [--reduce N] [--lenient] [--raw] [--time N] FILE...`
//!
//! Prints one line per file. `--raw` also writes `FILE.<n>.i32` per component
//! (little-endian samples) for comparison with another decoder; `--time N`
//! decodes N times and prints the fastest in milliseconds.

use std::time::Instant;

#[global_allocator]
static GLOBAL_ALLOC: rusty_alloc_api::RustyAlloc = rusty_alloc_api::RustyAlloc;

fn main() {
    let mut options = rusty_jpx::DecodeOptions::new();
    let (mut raw, mut time) = (None, 0u32);
    let mut args = std::env::args().skip(1);
    while let Some(a) = args.next() {
        match a.as_str() {
            "--reduce" => options.reduce = args.next().unwrap().parse().unwrap(),
            "--lenient" => options.allow_truncated = true,
            "--no-palette" => options.apply_palette = false,
            "--raw" => raw = args.next(),
            "--time" => time = args.next().unwrap().parse().unwrap(),
            file => {
                let data = std::fs::read(file).unwrap();
                let mut best = f64::MAX;
                let mut result = rusty_jpx::decode_jpx_with(&data, &options);
                for _ in 0..time {
                    let t = Instant::now();
                    result = rusty_jpx::decode_jpx_with(&data, &options);
                    best = best.min(t.elapsed().as_secs_f64() * 1e3);
                }
                match result {
                    Ok(j) => {
                        print!(
                            "{file} ok {}x{} {:?} alpha={} icc={} trunc={}",
                            j.width,
                            j.height,
                            j.color,
                            j.has_alpha,
                            j.icc.as_ref().map_or(0, Vec::len),
                            j.truncated
                        );
                        for c in &j.components {
                            let s = if c.signed { "s" } else { "u" };
                            print!(" {}x{}:{s}{}", c.width, c.height, c.bits);
                        }
                        if time > 0 {
                            print!(" {best:.3} ms");
                        }
                        println!();
                        for c in &j.concealed {
                            println!("  concealed: {c}");
                        }
                        if let Some(prefix) = &raw {
                            for (i, c) in j.components.iter().enumerate() {
                                let b: Vec<u8> =
                                    c.data.iter().flat_map(|v| v.to_le_bytes()).collect();
                                std::fs::write(format!("{prefix}.{i}.i32"), b).unwrap();
                            }
                        }
                    }
                    Err(e) => println!("{file} err {e}"),
                }
            }
        }
    }
}
