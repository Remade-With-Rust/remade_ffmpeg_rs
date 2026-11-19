//! Fixed workloads for the instruction-count (callgrind Ir) campaign.
//!
//! `icc_ir MODE` runs one workload. The measured work sits in functions
//! named `hot_*` (`#[inline(never)]`), so `valgrind --tool=callgrind
//! --toggle-collect='*hot_*'` counts exactly that and nothing of the setup.
//! Every run prints work-parity anchors: the pixel/byte count and an FNV-1a
//! of the output bytes. If an anchor moves, the experiment changed.
//!
//! Profiles: built-ins, plus Coated FOGRA39 from `RUSTY_ICC_FOGRA39`.

#[global_allocator]
static GLOBAL_ALLOC: rusty_alloc_api::RustyAlloc = rusty_alloc_api::RustyAlloc;

use rusty_icc::{Intent, Layout, Options, Profile, Transform};
use std::hint::black_box;

const SIDE: usize = 256;

fn fnv(bytes: impl IntoIterator<Item = u8>) -> u64 {
    let mut h = 0xcbf2_9ce4_8422_2325u64;
    for b in bytes {
        h = (h ^ u64::from(b)).wrapping_mul(0x100_0000_01b3);
    }
    h
}

/// Gradient + sinusoid + xorshift noise in unit values (the bench-lcms image
/// at a smaller size).
fn image(ch: usize) -> Vec<f64> {
    let mut s = 0x5EED_1CC0u64;
    let mut v = Vec::with_capacity(SIDE * SIDE * ch);
    for y in 0..SIDE {
        for x in 0..SIDE {
            let fx = x as f64 / SIDE as f64;
            let fy = y as f64 / SIDE as f64;
            let base = [
                fx,
                fy,
                0.5 + 0.5 * (std::f64::consts::TAU * (fx * 3.0 + fy * 2.0)).sin(),
                (fx * fy).sqrt(),
            ];
            for b in base.iter().take(ch) {
                s ^= s >> 12;
                s ^= s << 25;
                s ^= s >> 27;
                let n = (s.wrapping_mul(0x2545_F491_4F6C_DD1D) >> 11) as f64 / (1u64 << 53) as f64;
                v.push((b + 0.06 * (n - 0.5)).clamp(0.0, 1.0));
            }
        }
    }
    v
}

fn fogra() -> Profile {
    let path = std::env::var("RUSTY_ICC_FOGRA39").expect("RUSTY_ICC_FOGRA39");
    Profile::new_from_slice(&std::fs::read(path).expect("read FOGRA39")).expect("parse")
}

fn xf(src: &Profile, sl: Layout, dst: &Profile, dl: Layout) -> Transform {
    Transform::new(src, sl, dst, dl, Intent::RelativeColorimetric, Options::default()).unwrap()
}

#[inline(never)]
fn hot_transform_u8(t: &Transform, src: &[u8], dst: &mut [u8]) {
    t.transform(black_box(src), black_box(dst)).unwrap();
}

#[inline(never)]
fn hot_transform_u16(t: &Transform, src: &[u16], dst: &mut [u16]) {
    t.transform(black_box(src), black_box(dst)).unwrap();
}

#[inline(never)]
fn hot_transform_f32(t: &Transform, src: &[f32], dst: &mut [f32]) {
    t.transform(black_box(src), black_box(dst)).unwrap();
}

#[inline(never)]
fn hot_build(src: &Profile, sl: Layout, dst: &Profile, dl: Layout) -> Transform {
    xf(black_box(src), sl, black_box(dst), dl)
}

#[inline(never)]
fn hot_parse_encode(bytes: &[u8]) -> Vec<u8> {
    Profile::new_from_slice(black_box(bytes)).unwrap().encode()
}

fn run_u8(src: &Profile, sl: Layout, dst: &Profile, dl: Layout) {
    let t = xf(src, sl, dst, dl);
    let s: Vec<u8> = image(sl.channels()).iter().map(|&u| (u * 255.0).round() as u8).collect();
    let mut o = vec![0u8; SIDE * SIDE * dl.channels()];
    t.transform(&s[..sl.channels()], &mut o[..dl.channels()]).unwrap(); // warm lazy state
    hot_transform_u8(&t, &s, &mut o);
    println!("pixels {} fnv {:016x}", SIDE * SIDE, fnv(o.iter().copied()));
}

fn run_u16(src: &Profile, sl: Layout, dst: &Profile, dl: Layout) {
    let t = xf(src, sl, dst, dl);
    let s: Vec<u16> = image(sl.channels()).iter().map(|&u| (u * 65535.0).round() as u16).collect();
    let mut o = vec![0u16; SIDE * SIDE * dl.channels()];
    t.transform(&s[..sl.channels()], &mut o[..dl.channels()]).unwrap();
    hot_transform_u16(&t, &s, &mut o);
    println!("pixels {} fnv {:016x}", SIDE * SIDE, fnv(o.iter().flat_map(|v| v.to_le_bytes())));
}

fn run_f32(src: &Profile, sl: Layout, dst: &Profile, dl: Layout, lab_in: bool) {
    let t = xf(src, sl, dst, dl);
    let mut s: Vec<f32> = image(sl.channels()).iter().map(|&u| u as f32).collect();
    if lab_in {
        for px in s.chunks_exact_mut(3) {
            px[0] *= 100.0;
            px[1] = px[1] * 255.0 - 128.0;
            px[2] = px[2] * 255.0 - 128.0;
        }
    }
    let mut o = vec![0f32; SIDE * SIDE * dl.channels()];
    hot_transform_f32(&t, &s, &mut o);
    println!("pixels {} fnv {:016x}", SIDE * SIDE, fnv(o.iter().flat_map(|v| v.to_le_bytes())));
}

fn main() {
    let mode = std::env::args().nth(1).unwrap_or_default();
    let srgb = Profile::new_srgb();
    let p3 = Profile::new_display_p3();
    let lab = Profile::new_lab_identity();
    match mode.as_str() {
        "p3u8" => run_u8(&srgb, Layout::Rgb, &p3, Layout::Rgb),
        "p3u16" => run_u16(&srgb, Layout::Rgb, &p3, Layout::Rgb),
        "fogu8" => run_u8(&srgb, Layout::Rgb, &fogra(), Layout::Cmyk),
        "fog16" => run_u16(&fogra(), Layout::Cmyk, &srgb, Layout::Rgb),
        "labf32" => run_f32(&srgb, Layout::Rgb, &lab, Layout::Lab, false),
        "labinf32" => run_f32(&lab, Layout::Lab, &srgb, Layout::Rgb, true),
        "build3" | "build4" => {
            let f = fogra();
            let t = if mode == "build3" {
                hot_build(&srgb, Layout::Rgb, &f, Layout::Cmyk)
            } else {
                hot_build(&f, Layout::Cmyk, &srgb, Layout::Rgb)
            };
            // Anchor: the transform's output on a fixed ramp.
            let n = if mode == "build3" { 3 } else { 4 };
            let s: Vec<u16> = (0..n * 64).map(|i| (i * 1031 % 65536) as u16).collect();
            let mut o = vec![0u16; 64 * if mode == "build3" { 4 } else { 3 }];
            t.transform(&s, &mut o).unwrap();
            println!("pixels 64 fnv {:016x}", fnv(o.iter().flat_map(|v| v.to_le_bytes())));
        }
        "parse" => {
            let path = std::env::var("RUSTY_ICC_FOGRA39").expect("RUSTY_ICC_FOGRA39");
            let bytes = std::fs::read(path).unwrap();
            let out = hot_parse_encode(&bytes);
            println!("bytes {} fnv {:016x}", out.len(), fnv(out.iter().copied()));
        }
        _ => eprintln!("modes: p3u8 p3u16 fogu8 fog16 labf32 labinf32 build3 build4 parse"),
    }
}
