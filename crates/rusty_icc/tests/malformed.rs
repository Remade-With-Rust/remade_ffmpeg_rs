//! Malformed and hostile profiles: every case must return `Err` (or a
//! usable profile), never panic, and never allocate memory out of
//! proportion to the input. The allocation bound is measured by a counting
//! wrapper around the project allocator (`rusty_alloc`), so this test binary
//! runs its cases single-threaded inside one `#[test]` per concern.

mod common;

use common::{camera_lut16_lab, camera_mab_xyz, data, fogra39, Rng, DATA_PROFILES};
use rusty_icc::{Error, Intent, Layout, Options, Profile, Transform};
use std::alloc::{GlobalAlloc, Layout as AllocLayout};
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::Mutex;

struct Counting;

static CURRENT: AtomicUsize = AtomicUsize::new(0);
static PEAK: AtomicUsize = AtomicUsize::new(0);
static TRACK: AtomicBool = AtomicBool::new(false);
static SERIAL: Mutex<()> = Mutex::new(());

// SAFETY: every method forwards to `RustyAlloc` with the caller's own
// arguments, preserving the `GlobalAlloc` contract; the counters are
// plain atomics and never touch the returned memory.
unsafe impl GlobalAlloc for Counting {
    unsafe fn alloc(&self, layout: AllocLayout) -> *mut u8 {
        // SAFETY: same contract as the caller's.
        let p = unsafe { rusty_alloc_api::RustyAlloc.alloc(layout) };
        if !p.is_null() && TRACK.load(Ordering::Relaxed) {
            let now = CURRENT.fetch_add(layout.size(), Ordering::Relaxed) + layout.size();
            PEAK.fetch_max(now, Ordering::Relaxed);
        }
        p
    }
    unsafe fn dealloc(&self, ptr: *mut u8, layout: AllocLayout) {
        if TRACK.load(Ordering::Relaxed) {
            CURRENT.fetch_sub(
                layout.size().min(CURRENT.load(Ordering::Relaxed)),
                Ordering::Relaxed,
            );
        }
        // SAFETY: same contract as the caller's.
        unsafe { rusty_alloc_api::RustyAlloc.dealloc(ptr, layout) }
    }
    unsafe fn realloc(&self, ptr: *mut u8, layout: AllocLayout, new_size: usize) -> *mut u8 {
        // SAFETY: same contract as the caller's.
        let p = unsafe { rusty_alloc_api::RustyAlloc.realloc(ptr, layout, new_size) };
        if !p.is_null() && TRACK.load(Ordering::Relaxed) {
            if new_size >= layout.size() {
                let now = CURRENT.fetch_add(new_size - layout.size(), Ordering::Relaxed)
                    + (new_size - layout.size());
                PEAK.fetch_max(now, Ordering::Relaxed);
            } else {
                let d = (layout.size() - new_size).min(CURRENT.load(Ordering::Relaxed));
                CURRENT.fetch_sub(d, Ordering::Relaxed);
            }
        }
        p
    }
}

#[global_allocator]
static GLOBAL_ALLOC: Counting = Counting;

/// Parses `bytes` and returns the result with the peak heap growth the
/// parse caused (allocations made and still live at the peak).
fn parse_measured(bytes: &[u8]) -> (Result<Profile, Error>, usize) {
    CURRENT.store(0, Ordering::SeqCst);
    PEAK.store(0, Ordering::SeqCst);
    TRACK.store(true, Ordering::SeqCst);
    let r = Profile::new_from_slice(bytes);
    TRACK.store(false, Ordering::SeqCst);
    (r, PEAK.load(Ordering::SeqCst))
}

fn be32(v: u32) -> [u8; 4] {
    v.to_be_bytes()
}

/// A minimal valid header + tag table with the given tags (sig, offset,
/// size) and trailing data.
fn build(tags: &[([u8; 4], u32, u32)], tail: &[u8], declared: Option<u32>) -> Vec<u8> {
    let mut v = vec![0u8; 128];
    v[8..12].copy_from_slice(&be32(0x0440_0000));
    v[12..16].copy_from_slice(b"mntr");
    v[16..20].copy_from_slice(b"RGB ");
    v[20..24].copy_from_slice(b"XYZ ");
    v[36..40].copy_from_slice(b"acsp");
    v.extend_from_slice(&be32(tags.len() as u32));
    for (s, o, l) in tags {
        v.extend_from_slice(s);
        v.extend_from_slice(&be32(*o));
        v.extend_from_slice(&be32(*l));
    }
    v.extend_from_slice(tail);
    let size = declared.unwrap_or(v.len() as u32);
    v[0..4].copy_from_slice(&be32(size));
    v
}

fn expect_err(name: &str, bytes: &[u8]) {
    let (r, peak) = parse_measured(bytes);
    assert!(r.is_err(), "{name}: expected Err, got Ok");
    assert!(
        peak <= 64 * 1024 + 16 * bytes.len(),
        "{name}: peak {peak} B for {} B input",
        bytes.len()
    );
}

#[test]
fn structural_errors() {
    let _g = SERIAL.lock().unwrap();
    // Truncated headers: every length short of the tag count.
    let good = Profile::new_srgb().encode();
    for n in 0..132 {
        expect_err("truncated header", &good[..n]);
    }
    // Declared size beyond the data, or below the minimum.
    expect_err("size > len", &build(&[], &[], Some(10_000)));
    expect_err("size < 132", &build(&[], &[], Some(100)));
    let mut bad_magic = build(&[], &[], None);
    bad_magic[36] = b'x';
    expect_err("magic", &bad_magic);
    let mut v5 = build(&[], &[], None);
    v5[8] = 5;
    expect_err("iccMAX version", &v5);
    // Tag table larger than the profile / absurd count.
    let mut many = build(&[], &[], None);
    many[128..132].copy_from_slice(&be32(u32::MAX));
    expect_err("tag count u32::MAX", &many);
    let xyz = [
        b"XYZ ".as_slice(),
        &[0; 4],
        &[0, 0, 0xf6, 0xd6, 0, 1, 0, 0, 0, 0, 0xd3, 0x2d],
    ]
    .concat();
    // Offset past the end, size past the end, offset + size overflowing.
    expect_err(
        "offset past end",
        &build(&[(*b"wtpt", 10_000, 20)], &xyz, None),
    );
    expect_err("size past end", &build(&[(*b"wtpt", 144, 200)], &xyz, None));
    expect_err(
        "offset+size overflow",
        &build(&[(*b"wtpt", 144, u32::MAX)], &xyz, None),
    );
    expect_err(
        "offset u32::MAX",
        &build(&[(*b"wtpt", u32::MAX, 20)], &xyz, None),
    );
    expect_err(
        "tag smaller than type header",
        &build(&[(*b"wtpt", 144, 4)], &xyz, None),
    );
    // Zero-length / truncated curves.
    let curv_short = [b"curv".as_slice(), &[0; 4], &be32(1000)].concat();
    expect_err(
        "curv count past end",
        &build(&[(*b"rTRC", 144, 12)], &curv_short, None),
    );
    let para_bad = [b"para".as_slice(), &[0; 4], &[0, 9, 0, 0], &[0; 28]].concat();
    expect_err(
        "para function 9",
        &build(&[(*b"rTRC", 144, 40)], &para_bad, None),
    );
    let para_short = [b"para".as_slice(), &[0; 4], &[0, 4, 0, 0], &[0; 8]].concat();
    expect_err(
        "para truncated",
        &build(&[(*b"rTRC", 144, 20)], &para_short, None),
    );
}

fn lut16_header(inputs: u8, outputs: u8, grid: u8, in_n: u16, out_n: u16) -> Vec<u8> {
    let mut v = [b"mft2".as_slice(), &[0; 4], &[inputs, outputs, grid, 0]].concat();
    for i in 0..9 {
        v.extend_from_slice(&be32(if i % 4 == 0 { 0x10000 } else { 0 }));
    }
    v.extend_from_slice(&in_n.to_be_bytes());
    v.extend_from_slice(&out_n.to_be_bytes());
    v
}

#[test]
fn absurd_lut_sizes() {
    let _g = SERIAL.lock().unwrap();
    // 15 inputs x 255 grid points: the CLUT size overflows usize.
    let t = lut16_header(15, 15, 255, 2, 2);
    let n = t.len() as u32;
    expect_err("lut16 255^15 grid", &build(&[(*b"A2B0", 144, n)], &t, None));
    // Grids whose size fits in `usize` (3 or 4 inputs x 255 points: up to
    // 4.2e9 entries) but whose data is absent must fail before allocating;
    // 8 x 255 overflows.
    for (i, g) in [(8u8, 255u8), (4, 255), (3, 255)] {
        let t = [lut16_header(i, 3, g, 2, 2), vec![0; 64]].concat();
        let n = t.len() as u32;
        expect_err(
            "lut16 large grid, data absent",
            &build(&[(*b"A2B0", 144, n)], &t, None),
        );
    }
    // Grid of 0 / 1 point, zero / one table entries.
    for (g, a, b) in [
        (0u8, 2u16, 2u16),
        (1, 2, 2),
        (2, 0, 2),
        (2, 1, 2),
        (2, 2, 0),
        (2, 5000, 2),
    ] {
        let t = [lut16_header(3, 3, g, a, b), vec![0; 4096]].concat();
        let n = t.len() as u32;
        expect_err("lut16 degenerate", &build(&[(*b"A2B0", 144, n)], &t, None));
    }
    // mAB with a 255^15 CLUT and offsets past the tag.
    let mut mab = [b"mAB ".as_slice(), &[0; 4], &[15, 3, 0, 0]].concat();
    for off in [32u32, 0, 0, 44, 0] {
        mab.extend_from_slice(&be32(off));
    }
    mab.extend_from_slice(&[b"curv".as_slice(), &[0; 8]].concat()); // B curve 1 (identity)
    let mut clut = vec![255u8; 15];
    clut.extend_from_slice(&[0, 2, 0, 0, 0]);
    mab.extend_from_slice(&clut);
    let n = mab.len() as u32;
    expect_err("mAB 255^15 CLUT", &build(&[(*b"A2B0", 144, n)], &mab, None));
    let mut mab2 = [b"mAB ".as_slice(), &[0; 4], &[3, 3, 0, 0]].concat();
    for off in [9999u32, 0, 0, 0, 0] {
        mab2.extend_from_slice(&be32(off));
    }
    let n = mab2.len() as u32;
    expect_err(
        "mAB offset past tag",
        &build(&[(*b"A2B0", 144, n)], &mab2, None),
    );
    // mluc record count larger than the tag.
    let mluc = [b"mluc".as_slice(), &[0; 4], &be32(0x0fff_ffff), &be32(12)].concat();
    let n = mluc.len() as u32;
    expect_err("mluc count", &build(&[(*b"desc", 144, n)], &mluc, None));
}

#[test]
fn overlapping_tags_stay_bounded() {
    let _g = SERIAL.lock().unwrap();
    // 1000 tags all pointing into one 8 KiB lut16 with distinct sizes: each
    // distinct (offset, size) would be decoded, so the 4x-size budget must
    // stop it.
    let body = {
        let mut b = lut16_header(3, 3, 9, 2, 2);
        b.extend(std::iter::repeat_n(
            0u8,
            3 * 2 * 2 + 9 * 9 * 9 * 3 * 2 + 3 * 2 * 2,
        ));
        // Slack so every truncated size below still holds a whole lut16.
        b.extend([0u8; 64]);
        b
    };
    let n = 1000u32;
    let start = 132 + 12 * n;
    let tags: Vec<([u8; 4], u32, u32)> = (0..n)
        .map(|i| {
            let s = [b'T', (i / 256) as u8, (i % 256) as u8, b'x'];
            (s, start, body.len() as u32 - (i % 40))
        })
        .collect();
    let bytes = build(&tags, &body, None);
    let (r, peak) = parse_measured(&bytes);
    println!(
        "1000 overlapping lut16 tags: {:?}, peak {peak} B for {} B",
        r.as_ref().map(|_| ()),
        bytes.len()
    );
    assert_eq!(
        r.err(),
        Some(Error::Malformed("tag data exceeds 4x profile size"))
    );
    assert!(peak <= 64 * 1024 + 16 * bytes.len(), "peak {peak}");
}

fn layout_for(p: &Profile) -> Option<Layout> {
    use rusty_icc::ColorSpace::*;
    match p.header.color_space {
        Gray => Some(Layout::Gray),
        Rgb => Some(Layout::Rgb),
        Cmyk => Some(Layout::Cmyk),
        Lab => Some(Layout::Lab),
        _ => None,
    }
}

#[test]
fn mutation_fuzz_never_panics_and_stays_bounded() {
    let _g = SERIAL.lock().unwrap();
    let mut seeds: Vec<(String, Vec<u8>)> = DATA_PROFILES
        .iter()
        .map(|n| ((*n).to_string(), data(n)))
        .collect();
    seeds.push(("builtin sRGB".into(), Profile::new_srgb().encode()));
    seeds.push(("builtin Lab".into(), Profile::new_lab_identity().encode()));
    seeds.push(("camera lut16".into(), camera_lut16_lab().encode()));
    seeds.push(("camera mAB".into(), camera_mab_xyz().encode()));
    if let Some(f) = fogra39() {
        seeds.push(("FOGRA39".into(), f));
    }
    let srgb = Profile::new_srgb();
    let iters = 2500usize;
    let mut rng = Rng(0x1CC0_FFEE_D00D_F00D);
    let (mut total, mut ok, mut transforms) = (0usize, 0usize, 0usize);
    let mut worst_ratio = 0.0f64;
    for (name, seed) in &seeds {
        for it in 0..iters {
            let mut m = seed.clone();
            match rng.below(6) {
                0 => {
                    // Flip 1-8 random bytes.
                    for _ in 0..=rng.below(8) {
                        let i = rng.below(m.len());
                        m[i] ^= 1 << rng.below(8);
                    }
                }
                1 => m.truncate(rng.below(m.len())),
                2 => {
                    // Corrupt a tag-table field (offset or size) with a random u32.
                    let count = u32::from_be_bytes([m[128], m[129], m[130], m[131]]) as usize;
                    if count > 0 {
                        let e = 132 + 12 * rng.below(count) + 4 + 4 * rng.below(2);
                        if e + 4 <= m.len() {
                            let v = match rng.below(3) {
                                0 => rng.next() as u32,
                                1 => u32::MAX - rng.below(16) as u32,
                                _ => rng.below(m.len() + 64) as u32,
                            };
                            m[e..e + 4].copy_from_slice(&v.to_be_bytes());
                        }
                    }
                }
                3 => {
                    // Overwrite a random 4-byte word inside the tag data.
                    if m.len() > 140 {
                        let i = 132 + rng.below(m.len() - 136);
                        let v = if rng.below(2) == 0 {
                            u32::MAX
                        } else {
                            rng.next() as u32
                        };
                        m[i..i + 4].copy_from_slice(&v.to_be_bytes());
                    }
                }
                4 => {
                    // Small bytes (grid points, channel counts, precisions).
                    for _ in 0..=rng.below(4) {
                        let i = rng.below(m.len());
                        m[i] = [0u8, 1, 2, 3, 15, 16, 255][rng.below(7)];
                    }
                }
                _ => {
                    // Splice a chunk of the profile over another place.
                    let a = rng.below(m.len());
                    let b = rng.below(m.len());
                    let n = rng.below(64).min(m.len() - a.max(b));
                    let chunk = m[a..a + n].to_vec();
                    m[b..b + n].copy_from_slice(&chunk);
                }
            }
            total += 1;
            let (r, peak) = parse_measured(&m);
            let ratio = peak as f64 / m.len().max(1) as f64;
            if m.len() > 4096 {
                worst_ratio = worst_ratio.max(ratio);
            }
            assert!(
                peak <= 64 * 1024 + 16 * m.len(),
                "{name} iteration {it}: parse peak {peak} B for {} B",
                m.len()
            );
            let Ok(p) = r else { continue };
            ok += 1;
            // Re-encode and re-parse whatever was accepted.
            let _ = Profile::new_from_slice(&p.encode());
            // Exercise the transform paths on a sample of accepted profiles
            // (building a LUT transform samples a full device-link grid).
            if it % 16 == 0 {
                if let Some(l) = layout_for(&p) {
                    for intent in [
                        Intent::Perceptual,
                        Intent::RelativeColorimetric,
                        Intent::AbsoluteColorimetric,
                    ] {
                        let bpc = Options {
                            black_point_compensation: true,
                        };
                        if let Ok(t) = Transform::new(&p, l, &srgb, Layout::Rgb, intent, bpc) {
                            transforms += 1;
                            let px = l.channels();
                            let src8: Vec<u8> = (0..px * 9).map(|i| (i * 37 % 256) as u8).collect();
                            let mut o8 = vec![0u8; 27];
                            t.transform(&src8, &mut o8).unwrap();
                            let srcf: Vec<f32> =
                                src8.iter().map(|&v| f32::from(v) / 255.0).collect();
                            let mut of = vec![0f32; 27];
                            t.transform(&srcf, &mut of).unwrap();
                        }
                        if let Ok(t) = Transform::new(&srgb, Layout::Rgb, &p, l, intent, bpc) {
                            transforms += 1;
                            let mut o = vec![0u16; l.channels() * 3];
                            t.transform(
                                &[0u16, 0, 0, 65535, 65535, 65535, 1000, 30000, 60000],
                                &mut o,
                            )
                            .unwrap();
                        }
                    }
                }
            }
        }
    }
    println!(
        "mutation fuzz: {total} mutated profiles from {} seeds, {ok} still parsed, {transforms} transforms built and run, worst parse peak/input (inputs > 4 KiB) = {worst_ratio:.2}x",
        seeds.len()
    );
}
