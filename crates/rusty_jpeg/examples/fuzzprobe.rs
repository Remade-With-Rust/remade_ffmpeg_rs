//! Deterministic robustness sweep: does the decoder PANIC on malformed input?
//!
//! A decoder is a parser for hostile bytes. `Err` is a correct outcome for a
//! corrupt file; a panic is not, and in a library it aborts the caller's
//! process. This sweep answers whether we have any, before deciding how much
//! fuzzing infrastructure to build.
//!
//! Deterministic on purpose — a seeded mutation schedule reproduces exactly, so
//! a finding can be turned into a regression test instead of a bug report that
//! says "fuzzer found something once".
//!
//! Run: `cargo run --release -p rusty_jpeg --example fuzzprobe -- <seed.jpg>...`

use rusty_jpeg::decode::Decoder;
use std::io::Cursor;

fn xorshift(s: &mut u64) -> u64 {
    *s ^= *s << 13;
    *s ^= *s >> 7;
    *s ^= *s << 17;
    *s
}

/// Mutations chosen to hit a JPEG parser where it is structurally weakest:
/// marker bytes, segment lengths, and the entropy stream's end.
fn mutate(src: &[u8], kind: u64, rng: &mut u64) -> Vec<u8> {
    let mut v = src.to_vec();
    if v.len() < 8 {
        return v;
    }
    match kind % 6 {
        // Flip a single bit anywhere.
        0 => {
            let i = (xorshift(rng) as usize) % v.len();
            v[i] ^= 1 << (xorshift(rng) % 8);
        }
        // Truncate — the classic way to walk a decoder off the end.
        1 => {
            let n = (xorshift(rng) as usize) % v.len();
            v.truncate(n.max(2));
        }
        // Corrupt a segment length, so a header claims a size the file lacks.
        2 => {
            for i in 2..v.len().saturating_sub(4) {
                if v[i] == 0xFF && !matches!(v[i + 1], 0x00 | 0xD8 | 0xD9) {
                    v[i + 2] = xorshift(rng) as u8;
                    v[i + 3] = xorshift(rng) as u8;
                    break;
                }
            }
        }
        // Inject a stray marker into the entropy stream.
        3 => {
            let i = (xorshift(rng) as usize) % (v.len() - 2) + 1;
            v[i] = 0xFF;
            v[i + 1] = (xorshift(rng) % 0xFD) as u8 + 1;
        }
        // Scribble a run of random bytes.
        4 => {
            let i = (xorshift(rng) as usize) % v.len();
            let n = ((xorshift(rng) as usize) % 64).min(v.len() - i);
            for b in &mut v[i..i + n] {
                *b = xorshift(rng) as u8;
            }
        }
        // Zero a run — hits loop bounds that assume non-zero dimensions.
        _ => {
            let i = (xorshift(rng) as usize) % v.len();
            let n = ((xorshift(rng) as usize) % 32).min(v.len() - i);
            v[i..i + n].fill(0);
        }
    }
    v
}

fn main() {
    let files: Vec<String> = std::env::args().skip(1).collect();
    if files.is_empty() {
        eprintln!("usage: fuzzprobe <seed.jpg>...");
        std::process::exit(2);
    }
    let iters: usize = std::env::var("FUZZ_ITERS")
        .ok()
        .and_then(|v| v.parse().ok())
        .unwrap_or(20_000);

    let mut ok = 0u64;
    let mut err = 0u64;
    let mut panics = Vec::new();

    for path in &files {
        let Ok(src) = std::fs::read(path) else {
            eprintln!("skip {path}");
            continue;
        };
        for i in 0..iters {
            let mut rng = 0x9E3779B97F4A7C15u64 ^ (i as u64).wrapping_mul(0x0F1B_2C3D);
            xorshift(&mut rng);
            let data = mutate(&src, i as u64, &mut rng);

            // Both entry points: the packed and the planar path decode through
            // different output assembly and have historically diverged.
            let planar = i % 2 == 0;
            let res = std::panic::catch_unwind(|| {
                let mut d = Decoder::new(Cursor::new(&data));
                d.set_single_threaded(true);
                if planar {
                    d.decode_planar().map(|p| p.components.len())
                } else {
                    d.decode().map(|p| p.len())
                }
            });
            match res {
                Ok(Ok(_)) => ok += 1,
                Ok(Err(_)) => err += 1,
                Err(_) => {
                    panics.push((path.clone(), i, planar, data.len()));
                    if panics.len() > 20 {
                        break;
                    }
                }
            }
        }
    }

    println!(
        "{} seeds x {iters} mutations: {ok} decoded, {err} rejected, {} PANICS",
        files.len(),
        panics.len()
    );
    for (f, i, planar, n) in panics.iter().take(20) {
        println!(
            "  PANIC  seed={} case={i} path={} bytes={n}",
            f,
            if *planar { "planar" } else { "packed" }
        );
    }
    if !panics.is_empty() {
        std::process::exit(1);
    }
}
