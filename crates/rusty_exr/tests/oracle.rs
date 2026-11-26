//! Regression oracle: small files written by OpenEXR C++ 3.5.5 in every
//! compression (scanline, tiled, mipmapped, multi-part), each channel
//! hashed by OpenEXR's own decoder (`tests/data/expected.txt`, produced
//! with the oracle tool described in the crate README). Every channel this
//! crate decodes must hash the same. The content is synthetic (generated
//! for this crate), so the fixtures carry no third-party licence.
//!
//! Also a hostile-input sweep over the same files: mutated, truncated and
//! spliced copies must return errors, never panic.

use std::collections::BTreeSet;
use std::path::Path;

fn fnv(b: &[u8]) -> u64 {
    let mut h = 0xcbf2_9ce4_8422_2325u64;
    for &x in b {
        h ^= u64::from(x);
        h = h.wrapping_mul(0x0000_0100_0000_01b3);
    }
    h
}

fn data_dir() -> &'static Path {
    Path::new(concat!(env!("CARGO_MANIFEST_DIR"), "/tests/data"))
}

fn hash_lines(name: &str, bytes: &[u8]) -> BTreeSet<String> {
    let d = rusty_exr::Decoder::new(bytes).unwrap_or_else(|e| panic!("{name}: {e}"));
    let mut out = BTreeSet::new();
    for p in 0..d.part_count() {
        let (nx, ny) = d.level_count(p).unwrap();
        let mip = d.header(p).unwrap().tiles().map(|t| t.level_mode)
            == Some(rusty_exr::LevelMode::Mipmap);
        for ly in 0..ny {
            for lx in 0..nx {
                if mip && lx != ly {
                    continue;
                }
                let img = d
                    .decode_level(p, lx, ly)
                    .unwrap_or_else(|e| panic!("{name} part {p} level {lx},{ly}: {e}"));
                for c in &img.channels {
                    let (b, t): (Vec<u8>, &str) = match &c.samples {
                        rusty_exr::Samples::F16(v) => {
                            (v.iter().flat_map(|x| x.to_le_bytes()).collect(), "F16")
                        }
                        rusty_exr::Samples::F32(v) => {
                            (v.iter().flat_map(|x| x.to_le_bytes()).collect(), "F32")
                        }
                        rusty_exr::Samples::U32(v) => {
                            (v.iter().flat_map(|x| x.to_le_bytes()).collect(), "U32")
                        }
                    };
                    out.insert(format!(
                        "{name} part {p} level {lx} {ly} channel {} type {t} w {} h {} fnv {:016x}",
                        c.desc.name,
                        c.width,
                        c.height,
                        fnv(&b)
                    ));
                }
            }
        }
    }
    out
}

#[test]
fn decodes_like_openexr() {
    let expected: BTreeSet<String> = std::fs::read_to_string(data_dir().join("expected.txt"))
        .unwrap()
        .lines()
        .filter(|l| !l.trim().is_empty())
        .map(str::to_string)
        .collect();
    let mut files: Vec<_> = std::fs::read_dir(data_dir())
        .unwrap()
        .filter_map(|e| e.ok().map(|e| e.path()))
        .filter(|p| p.extension().is_some_and(|x| x == "exr"))
        .collect();
    files.sort();
    assert!(files.len() >= 10, "fixtures missing");
    let mut got = BTreeSet::new();
    for f in &files {
        let name = f.file_name().unwrap().to_str().unwrap().to_string();
        got.extend(hash_lines(&name, &std::fs::read(f).unwrap()));
    }
    let missing: Vec<_> = expected.difference(&got).collect();
    let extra: Vec<_> = got.difference(&expected).collect();
    assert!(
        missing.is_empty() && extra.is_empty(),
        "differs from OpenEXR:\nexpected but not decoded: {missing:#?}\ndecoded but not expected: {extra:#?}"
    );
}

struct Rng(u64);
impl Rng {
    fn next(&mut self) -> u64 {
        self.0 ^= self.0 << 13;
        self.0 ^= self.0 >> 7;
        self.0 ^= self.0 << 17;
        self.0
    }
}

#[test]
fn hostile_input_never_panics() {
    let mut files: Vec<Vec<u8>> = std::fs::read_dir(data_dir())
        .unwrap()
        .filter_map(|e| e.ok().map(|e| e.path()))
        .filter(|p| p.extension().is_some_and(|x| x == "exr"))
        .map(|p| std::fs::read(p).unwrap())
        .collect();
    files.sort();
    let limits = rusty_exr::Limits {
        max_image_bytes: 64 << 20,
        max_chunk_bytes: 16 << 20,
        ..rusty_exr::Limits::default()
    };
    let iters = if cfg!(debug_assertions) { 600 } else { 6000 };
    let mut rng = Rng(0x9e37_79b9_7f4a_7c15);
    for it in 0..iters {
        let src = &files[it % files.len()];
        let mut d = src.clone();
        match rng.next() % 4 {
            0 => {
                for _ in 0..1 + rng.next() % 8 {
                    let i = (rng.next() as usize) % d.len();
                    d[i] ^= 1 << (rng.next() % 8);
                }
            }
            1 => d.truncate((rng.next() as usize) % d.len()),
            2 => {
                let i = (rng.next() as usize) % d.len().min(2048);
                for k in i..(i + 1 + (rng.next() as usize) % 16).min(d.len()) {
                    d[k] = rng.next() as u8;
                }
            }
            _ => {
                if d.len() > 8 {
                    let i = (rng.next() as usize) % (d.len() - 4);
                    let v: u32 =
                        [0, 1, 0x7fff_ffff, 0x8000_0000, 0xffff_ffff][(rng.next() % 5) as usize];
                    d[i..i + 4].copy_from_slice(&v.to_le_bytes());
                }
            }
        }
        // Any result is fine; a panic fails the test (with its iteration).
        if let Ok(dec) = rusty_exr::Decoder::with_limits(&d, limits) {
            let dec = dec.threads(rusty_exr::Threads::Single);
            for p in 0..dec.part_count() {
                let _ = dec.decode_part(p);
                for c in 0..dec.chunk_count(p).unwrap_or(0).min(4) {
                    let _ = dec.decode_chunk(p, c);
                }
            }
        }
    }
}
