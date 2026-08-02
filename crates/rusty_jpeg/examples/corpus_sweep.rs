//! Run a directory of JPEGs through both decode paths and classify the outcome.
//!
//! Built after a panic on progressive files reached two published releases. The
//! fix was three lines; the reason it shipped was that no fixture in the tree
//! had the shape that triggers it — and no fixture could, because this crate's
//! own encoder cannot produce that layout. A corpus from FOREIGN encoders, run
//! through the real decoder with outcomes classified, is what makes "no panics"
//! a measurement instead of a hope.
//!
//! Every file is decoded twice, through `decode()` and `decode_planar()`, since
//! those assemble output differently and have diverged before.
//!
//! Usage: corpus_sweep <dir> [mutations-per-file]

use rusty_jpeg::decode::Decoder;
use std::io::Cursor;

fn xorshift(s: &mut u64) -> u64 {
    *s ^= *s << 13;
    *s ^= *s >> 7;
    *s ^= *s << 17;
    *s
}

fn mutate(src: &[u8], kind: u64, rng: &mut u64) -> Vec<u8> {
    let mut v = src.to_vec();
    if v.len() < 8 {
        return v;
    }
    match kind % 5 {
        0 => {
            let i = (xorshift(rng) as usize) % v.len();
            v[i] ^= 1 << (xorshift(rng) % 8);
        }
        1 => {
            let n = (xorshift(rng) as usize) % v.len();
            v.truncate(n.max(2));
        }
        2 => {
            // Corrupt a segment length: the classic way to make a header claim
            // a size the file does not have.
            for i in 2..v.len().saturating_sub(4) {
                if v[i] == 0xFF && !matches!(v[i + 1], 0x00 | 0xD8 | 0xD9) {
                    v[i + 2] = xorshift(rng) as u8;
                    v[i + 3] = xorshift(rng) as u8;
                    break;
                }
            }
        }
        3 => {
            let i = (xorshift(rng) as usize) % (v.len() - 2) + 1;
            v[i] = 0xFF;
            v[i + 1] = (xorshift(rng) % 0xFD) as u8 + 1;
        }
        _ => {
            let i = (xorshift(rng) as usize) % v.len();
            let n = ((xorshift(rng) as usize) % 48).min(v.len() - i);
            for b in &mut v[i..i + n] {
                *b = xorshift(rng) as u8;
            }
        }
    }
    v
}

fn main() {
    let mut a = std::env::args().skip(1);
    let dir = a.next().expect("usage: corpus_sweep <dir> [mutations]");
    let muts: usize = a.next().and_then(|v| v.parse().ok()).unwrap_or(0);

    let mut files: Vec<_> = std::fs::read_dir(&dir)
        .expect("read dir")
        .filter_map(|e| e.ok().map(|e| e.path()))
        .filter(|p| p.extension().is_some_and(|e| e == "jpg" || e == "jpeg"))
        .collect();
    files.sort();

    let (mut ok, mut err, mut panics) = (0u64, 0u64, Vec::new());

    let run =
        |data: &[u8], label: String, panics: &mut Vec<String>, ok: &mut u64, err: &mut u64| {
            for planar in [false, true] {
                let res = std::panic::catch_unwind(|| {
                    let mut d = Decoder::new(Cursor::new(data));
                    d.set_single_threaded(true);
                    d.set_max_decoding_buffer_size(256 * 1024 * 1024);
                    if planar {
                        d.decode_planar().map(|p| p.components.len())
                    } else {
                        d.decode().map(|p| p.len())
                    }
                });
                match res {
                    Ok(Ok(_)) => *ok += 1,
                    Ok(Err(_)) => *err += 1,
                    Err(_) => panics.push(format!(
                        "{label} [{}]",
                        if planar { "planar" } else { "packed" }
                    )),
                }
            }
        };

    // Silence the default panic printer; `catch_unwind` is doing the reporting.
    let prev = std::panic::take_hook();
    std::panic::set_hook(Box::new(|_| {}));

    for path in &files {
        let Ok(data) = std::fs::read(path) else {
            continue;
        };
        let name = path.file_name().unwrap().to_string_lossy().to_string();
        run(&data, name.clone(), &mut panics, &mut ok, &mut err);
        for m in 0..muts {
            let mut rng = 0x9E3779B97F4A7C15u64 ^ (m as u64).wrapping_mul(0x0F1B_2C3D);
            xorshift(&mut rng);
            let d = mutate(&data, m as u64, &mut rng);
            run(&d, format!("{name}#mut{m}"), &mut panics, &mut ok, &mut err);
        }
    }
    std::panic::set_hook(prev);

    println!(
        "{} files x (1 + {muts} mutations) x 2 paths: {ok} decoded, {err} rejected, {} PANICS",
        files.len(),
        panics.len()
    );
    for p in panics.iter().take(20) {
        println!("  PANIC {p}");
    }
    if !panics.is_empty() {
        std::process::exit(1);
    }
}
