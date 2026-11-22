//! `Precision::Fast` against the exact path: the CIEDE2000 budget of the
//! LUT-chain fast path (float CMYK), on Coated FOGRA39 (from
//! `RUSTY_ICC_FOGRA39_V4` and/or `RUSTY_ICC_FOGRA39`), dense synthetic sweeps
//! and, when `RUSTY_ICC_PHOTO` points at an 8-bit binary PPM, a photograph.
//!
//! `cargo test --release --test fast_precision -- --ignored --nocapture`

use rusty_icc::color::{delta_e_2000, Lab};
use rusty_icc::{Intent, Layout, Options, Precision, Profile, Transform};

/// The budget: CIEDE2000 between the fast and exact outputs (both taken to
/// Lab by the exact path), worst and mean.
const MAX_DE: f64 = 1.0;
const MEAN_DE: f64 = 0.1;

fn fogras() -> Vec<(String, Profile)> {
    ["RUSTY_ICC_FOGRA39_V4", "RUSTY_ICC_FOGRA39"]
        .iter()
        .filter_map(|k| {
            let path = std::env::var(k).ok()?;
            let p = Profile::new_from_slice(&std::fs::read(&path).ok()?).ok()?;
            Some(((*k).to_string(), p))
        })
        .collect()
}

/// The photograph's pixels as `k / 255` floats.
fn photo() -> Option<Vec<f32>> {
    let ppm = std::fs::read(std::env::var("RUSTY_ICC_PHOTO").ok()?).ok()?;
    let mut fields = 0;
    let mut i = 0;
    let mut dims = Vec::new();
    while fields < 4 && i < ppm.len() {
        while ppm[i].is_ascii_whitespace() {
            i += 1;
        }
        let start = i;
        while !ppm[i].is_ascii_whitespace() {
            i += 1;
        }
        if fields > 0 {
            dims.push(
                std::str::from_utf8(&ppm[start..i])
                    .ok()?
                    .parse::<usize>()
                    .ok()?,
            );
        }
        fields += 1;
    }
    let n = dims[0] * dims[1] * 3;
    let px = ppm.get(i + 1..i + 1 + n)?;
    Some(px.iter().map(|&v| f32::from(v) / 255.0).collect())
}

fn xorshift(s: &mut u64) -> u64 {
    *s ^= *s << 13;
    *s ^= *s >> 7;
    *s ^= *s << 17;
    *s
}

/// A `k / steps` lattice over `ch` channels, then `random` pseudo-random
/// points.
fn sweep(ch: usize, steps: usize, random: usize) -> Vec<f32> {
    let n = (steps + 1).pow(ch as u32);
    let mut v = Vec::with_capacity((n + random) * ch);
    for p in 0..n {
        let mut r = p;
        for _ in 0..ch {
            v.push((r % (steps + 1)) as f32 / steps as f32);
            r /= steps + 1;
        }
    }
    let mut s = 0x2545_F491_4F6C_DD1Du64;
    for _ in 0..random * ch {
        v.push((xorshift(&mut s) >> 40) as f32 / (1u64 << 24) as f32);
    }
    v
}

/// The photograph as CMYK (naive: K from the brightest channel).
fn to_cmyk(rgb: &[f32]) -> Vec<f32> {
    rgb.chunks_exact(3)
        .flat_map(|p| {
            let k = 1.0 - p[0].max(p[1]).max(p[2]);
            let d = (1.0 - k).max(1e-6);
            [
                (1.0 - p[0] - k) / d,
                (1.0 - p[1] - k) / d,
                (1.0 - p[2] - k) / d,
                k,
            ]
        })
        .collect()
}

/// Worst and mean CIEDE2000 between two device colour sets, through the
/// exact path of `profile` (`layout`) to Lab.
fn de(profile: &Profile, layout: Layout, a: &[f32], b: &[f32]) -> (f64, f64) {
    let lab = Profile::new_lab_identity();
    let t = Transform::new(
        profile,
        layout,
        &lab,
        Layout::Lab,
        Intent::RelativeColorimetric,
        Options::default(),
    )
    .unwrap();
    let n = a.len() / layout.channels();
    let (mut la, mut lb) = (vec![0f32; n * 3], vec![0f32; n * 3]);
    t.transform(a, &mut la).unwrap();
    t.transform(b, &mut lb).unwrap();
    let mut worst = 0f64;
    let mut sum = 0f64;
    for (x, y) in la.chunks_exact(3).zip(lb.chunks_exact(3)) {
        let e = delta_e_2000(
            Lab::new(x[0].into(), x[1].into(), x[2].into()),
            Lab::new(y[0].into(), y[1].into(), y[2].into()),
        );
        worst = worst.max(e);
        sum += e;
    }
    (worst, sum / n as f64)
}

/// One direction: fast against exact on `src`, the CIEDE2000 checked and
/// printed.
fn check(
    name: &str,
    (from, sl): (&Profile, Layout),
    (to, dl): (&Profile, Layout),
    intent: Intent,
    src: &[f32],
) -> (f64, f64) {
    let exact = Transform::new(from, sl, to, dl, intent, Options::default()).unwrap();
    let fast = Transform::new(from, sl, to, dl, intent, Options::default())
        .unwrap()
        .with_precision(Precision::Fast);
    let n = src.len() / sl.channels();
    let (mut a, mut b) = (vec![0f32; n * dl.channels()], vec![0f32; n * dl.channels()]);
    exact.transform(src, &mut a).unwrap();
    fast.transform(src, &mut b).unwrap();
    let (worst, mean) = de(to, dl, &a, &b);
    println!("{name}: {n} colours, dE2000 max {worst:.3} mean {mean:.4}");
    (worst, mean)
}

#[test]
#[ignore = "needs RUSTY_ICC_FOGRA39_V4 / RUSTY_ICC_FOGRA39; several seconds"]
fn fast_chain_cmyk_budget() {
    let fogras = fogras();
    if fogras.is_empty() {
        println!("Coated FOGRA39 not available; skipped");
        return;
    }
    let srgb = Profile::new_srgb();
    let photo = photo();
    // A 65^3 RGB lattice and a 33^4 CMYK one (each through the profiles'
    // CLUT points, cell midpoints and more), plus 10^6 random colours.
    let rgb_sweep = sweep(3, 64, 1_000_000);
    let cmyk_sweep = sweep(4, 32, 1_000_000);
    let mut worst = (0f64, 0f64);
    for (key, fogra) in &fogras {
        for intent in [Intent::Perceptual, Intent::RelativeColorimetric] {
            let mut sets: Vec<(&str, Vec<f32>, Vec<f32>)> =
                vec![("sweep", rgb_sweep.clone(), cmyk_sweep.clone())];
            if let Some(p) = &photo {
                sets.push(("photo", p.clone(), to_cmyk(p)));
            }
            for (set, rgb, cmyk) in &sets {
                let n = format!("sRGB -> {key} {intent:?} {set}");
                let r = check(&n, (&srgb, Layout::Rgb), (fogra, Layout::Cmyk), intent, rgb);
                worst = (worst.0.max(r.0), worst.1.max(r.1));
                let n = format!("{key} -> sRGB {intent:?} {set}");
                let r = check(
                    &n,
                    (fogra, Layout::Cmyk),
                    (&srgb, Layout::Rgb),
                    intent,
                    cmyk,
                );
                worst = (worst.0.max(r.0), worst.1.max(r.1));
            }
        }
    }
    println!(
        "fast chain: worst dE2000 max {:.3}, worst mean {:.4}",
        worst.0, worst.1
    );
    assert!(worst.0 <= MAX_DE && worst.1 <= MEAN_DE, "budget: {worst:?}");
}
