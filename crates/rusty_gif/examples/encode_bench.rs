//! End-to-end single-frame GIF encode bench on raw RGB24 input.
//! Policy comes from RUSTY_GIF_Q_POLICY (see quantize.rs); prints best-of-N
//! encode time and writes the encoded GIF for external quality scoring.
//!
//! Usage: encode_bench <file.rgb> <width> <height> <out.gif> [reps]

use std::time::Instant;

use rusty_gif::{Encoder, Frame};

// Project convention: every encoder-carrying binary runs under rusty_alloc.
#[global_allocator]
static GLOBAL_ALLOC: rusty_alloc_api::RustyAlloc = rusty_alloc_api::RustyAlloc;

fn main() {
    let mut args = std::env::args().skip(1);
    let path = args.next().expect("usage: encode_bench <file.rgb> <w> <h> <out.gif> [reps]");
    let w: u16 = args.next().unwrap().parse().unwrap();
    let h: u16 = args.next().unwrap().parse().unwrap();
    let out_path = args.next().expect("missing <out.gif>");
    let reps: usize = args.next().map(|r| r.parse().unwrap()).unwrap_or(3);

    let rgb = std::fs::read(&path).unwrap();
    assert_eq!(rgb.len(), w as usize * h as usize * 3, "bad dimensions");

    let mut best = f64::MAX;
    let mut out = Vec::new();
    for _ in 0..reps {
        let t = Instant::now();
        let frame = Frame::from_rgb(w, h, &rgb);
        let mut buf = Vec::new();
        let mut enc = Encoder::new(&mut buf, w, h, &[]).unwrap();
        enc.write_frame(&frame).unwrap();
        drop(enc);
        let dt = t.elapsed().as_secs_f64();
        if dt < best {
            best = dt;
        }
        out = buf;
    }
    std::fs::write(&out_path, &out).unwrap();

    let name = std::path::Path::new(&path).file_stem().unwrap().to_string_lossy();
    let policy = std::env::var("RUSTY_GIF_Q_POLICY").unwrap_or_else(|_| "default".into());
    println!("{name} policy={policy} best {:.1} ms  bytes {}", best * 1e3, out.len());
}
