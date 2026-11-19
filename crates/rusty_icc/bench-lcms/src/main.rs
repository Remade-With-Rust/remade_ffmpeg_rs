//! rusty_icc against LittleCMS 2: accuracy grid, black points, speed.
//!
//! `bench-lcms accuracy`     grid comparison table (markdown on stdout)
//! `bench-lcms blackpoints`  black-point detection, rusty_icc vs lcms2
//! `bench-lcms speed CASE N` side-by-side timing (see README.md)
//!
//! Profiles: `RUSTY_ICC_DATA` (default `../tests/data`) and
//! `RUSTY_ICC_FOGRA39` (Coated FOGRA39, not committed).

#![allow(clippy::too_many_arguments, clippy::cast_precision_loss)]

#[global_allocator]
static GLOBAL_ALLOC: rusty_alloc_api::RustyAlloc = rusty_alloc_api::RustyAlloc;

#[path = "../../tests/common/mod.rs"]
mod common;
mod lcms;

use lcms::{Prof, Xf};
use rusty_icc::color::{delta_e_2000, xyz_to_lab, Lab, Xyz, D50};
use rusty_icc::{Intent, Layout, Lut, Options, Profile, Tag, Transform};
use std::path::PathBuf;
use std::time::Instant;

fn data(name: &str) -> Vec<u8> {
    let dir = std::env::var_os("RUSTY_ICC_DATA").map_or_else(
        || PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../tests/data"),
        PathBuf::from,
    );
    std::fs::read(dir.join(name)).unwrap_or_else(|e| panic!("{name}: {e}"))
}

fn fogra() -> Option<Vec<u8>> {
    std::fs::read(std::env::var_os("RUSTY_ICC_FOGRA39")?).ok()
}

#[derive(Clone)]
enum End {
    Bytes(Vec<u8>),
    Lab,
}

impl End {
    fn ours(&self) -> Profile {
        match self {
            Self::Bytes(b) => Profile::new_from_slice(b).expect("rusty_icc parse"),
            Self::Lab => Profile::new_lab_identity(),
        }
    }
    fn lcms(&self) -> Prof {
        match self {
            Self::Bytes(b) => Prof::from_mem(b),
            Self::Lab => Prof::lab4(),
        }
    }
}

#[derive(Clone, Copy, PartialEq, Debug)]
enum Space {
    Rgb,
    Cmyk,
    Gray,
    Lab,
}

impl Space {
    const fn ch(self) -> usize {
        match self {
            Self::Gray => 1,
            Self::Rgb | Self::Lab => 3,
            Self::Cmyk => 4,
        }
    }
    const fn layout(self) -> Layout {
        match self {
            Self::Rgb => Layout::Rgb,
            Self::Cmyk => Layout::Cmyk,
            Self::Gray => Layout::Gray,
            Self::Lab => Layout::Lab,
        }
    }
    fn fmt(self, ty: Ty) -> u32 {
        use lcms::*;
        let t = [
            [TYPE_RGB_8, TYPE_RGB_16, TYPE_RGB_FLT, TYPE_RGB_DBL],
            [TYPE_CMYK_8, TYPE_CMYK_16, TYPE_CMYK_FLT, TYPE_CMYK_DBL],
            [TYPE_GRAY_8, TYPE_GRAY_16, TYPE_GRAY_FLT, TYPE_GRAY_DBL],
            [TYPE_LAB_8, TYPE_LAB_16, TYPE_LAB_FLT, TYPE_LAB_DBL],
        ];
        let row = match self {
            Self::Rgb => 0,
            Self::Cmyk => 1,
            Self::Gray => 2,
            Self::Lab => 3,
        };
        t[row][match ty {
            Ty::U8 => 0,
            Ty::U16 => 1,
            Ty::F32 | Ty::F32Exact => 2,
        }]
    }
}

#[derive(Clone, Copy, PartialEq, Debug)]
enum Ty {
    U8,
    U16,
    /// lcms2 with its default optimisation.
    F32,
    /// lcms2 with `cmsFLAGS_NOOPTIMIZE` (its exact pipeline).
    F32Exact,
}

const fn lcms_intent(i: Intent) -> u32 {
    i as u32
}

/// Unit value (`0..1`, v4-normalised for Lab) to the natural `f32` value
/// for this space, as each engine expects it.
fn unit_to_f32(space: Space, c: usize, u: f64, lcms_side: bool) -> f32 {
    match space {
        Space::Lab => match c {
            0 => (u * 100.0) as f32,
            _ => (u * 255.0 - 128.0) as f32,
        },
        Space::Cmyk if lcms_side => (u * 100.0) as f32,
        _ => u as f32,
    }
}

fn f32_to_unit(space: Space, c: usize, v: f32, lcms_side: bool) -> f64 {
    let v = f64::from(v);
    match space {
        Space::Lab => match c {
            0 => v / 100.0,
            _ => (v + 128.0) / 255.0,
        },
        Space::Cmyk if lcms_side => v / 100.0,
        _ => v,
    }
}

static UNBOUNDED: std::sync::atomic::AtomicUsize = std::sync::atomic::AtomicUsize::new(0);

fn grid(ch: usize, n: usize) -> Vec<f64> {
    let total = n.pow(ch as u32);
    let mut v = Vec::with_capacity(total * ch);
    for idx in 0..total {
        let mut rem = idx;
        let mut px = vec![0.0; ch];
        for c in (0..ch).rev() {
            px[c] = (rem % n) as f64 / (n - 1) as f64;
            rem /= n;
        }
        v.extend(px);
    }
    v
}

struct Case {
    name: String,
    src: End,
    src_space: Space,
    dst: End,
    dst_space: Space,
    proof: Option<(End, Intent)>,
    intent: Intent,
    bpc: bool,
    n: usize,
}

struct Row {
    name: String,
    ty: Ty,
    colours: usize,
    max_diff: f64,
    mean_diff: f64,
    mean_de: f64,
    max_de: f64,
    /// Output values lcms2 left outside `0..=1` (float only), compared
    /// after clamping.
    unbounded: usize,
    /// (max, mean) of rusty_icc's integer output vs lcms2's exact pipeline.
    ours_exact: (f64, f64),
    /// (max, mean) of lcms2's integer output vs its own exact pipeline.
    lcms_exact: (f64, f64),
}

/// Runs both engines over the case's grid for one sample type and returns
/// (ours, lcms) outputs in unit values.
fn run_case(c: &Case, ty: Ty) -> (Vec<f64>, Vec<f64>) {
    let units = grid(c.src_space.ch(), c.n);
    let px = units.len() / c.src_space.ch();
    let (so, do_) = (c.src.ours(), c.dst.ours());
    let (sl, dl) = (c.src.lcms(), c.dst.lcms());
    let opts = Options {
        black_point_compensation: c.bpc,
    };
    let ours = match &c.proof {
        None => Transform::new(
            &so,
            c.src_space.layout(),
            &do_,
            c.dst_space.layout(),
            c.intent,
            opts,
        ),
        Some((p, pi)) => Transform::new_proofing(
            &so,
            c.src_space.layout(),
            &p.ours(),
            &do_,
            c.dst_space.layout(),
            c.intent,
            *pi,
            opts,
        ),
    }
    .expect("rusty_icc transform");
    let mut flags = if c.bpc { lcms::FLAGS_BPC } else { 0 };
    if ty == Ty::F32Exact {
        flags |= lcms::FLAGS_NOOPTIMIZE;
    }
    let (fin, fout) = (c.src_space.fmt(ty), c.dst_space.fmt(ty));
    let proof_prof = c.proof.as_ref().map(|(p, _)| p.lcms());
    let theirs = match (&c.proof, &proof_prof) {
        (Some((_, pi)), Some(pp)) => Xf::proofing(
            &sl,
            fin,
            &dl,
            fout,
            pp,
            lcms_intent(c.intent),
            lcms_intent(*pi),
            flags,
        ),
        _ => Xf::new(&sl, fin, &dl, fout, lcms_intent(c.intent), flags),
    };
    let dc = c.dst_space.ch();
    let sc = c.src_space.ch();
    match ty {
        Ty::U8 => {
            let s: Vec<u8> = units.iter().map(|&u| (u * 255.0).round() as u8).collect();
            let (mut a, mut b) = (vec![0u8; px * dc], vec![0u8; px * dc]);
            ours.transform(&s, &mut a).unwrap();
            theirs.run(&s, &mut b);
            let f = |v: &[u8]| v.iter().map(|&x| f64::from(x) / 255.0).collect();
            (f(&a), f(&b))
        }
        Ty::U16 => {
            let s: Vec<u16> = units
                .iter()
                .map(|&u| (u * 65535.0).round() as u16)
                .collect();
            let (mut a, mut b) = (vec![0u16; px * dc], vec![0u16; px * dc]);
            ours.transform(&s, &mut a).unwrap();
            theirs.run(&s, &mut b);
            let f = |v: &[u16]| v.iter().map(|&x| f64::from(x) / 65535.0).collect();
            (f(&a), f(&b))
        }
        Ty::F32 | Ty::F32Exact => {
            let so: Vec<f32> = units
                .iter()
                .enumerate()
                .map(|(i, &u)| unit_to_f32(c.src_space, i % sc, u, false))
                .collect();
            let sl: Vec<f32> = units
                .iter()
                .enumerate()
                .map(|(i, &u)| unit_to_f32(c.src_space, i % sc, u, true))
                .collect();
            let (mut a, mut b) = (vec![0f32; px * dc], vec![0f32; px * dc]);
            ours.transform(&so, &mut a).unwrap();
            theirs.run(&sl, &mut b);
            let a = a
                .iter()
                .enumerate()
                .map(|(i, &v)| f32_to_unit(c.dst_space, i % dc, v, false))
                .collect();
            let b = b
                .iter()
                .enumerate()
                .map(|(i, &v)| {
                    let u = f32_to_unit(c.dst_space, i % dc, v, true);
                    // lcms2 float transforms are unbounded (matrix-shaper
                    // outputs keep out-of-gamut values outside 0..1);
                    // rusty_icc clamps device outputs. Compare clamped and
                    // count how often lcms2 went outside.
                    if c.dst_space != Space::Lab && !(-1e-6..=1.0 + 1e-6).contains(&u) {
                        UNBOUNDED.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
                    }
                    if c.dst_space == Space::Lab {
                        u
                    } else {
                        u.clamp(0.0, 1.0)
                    }
                })
                .collect();
            (a, b)
        }
    }
}

/// Lab (D50) of unit device values through `dst` with lcms2's exact
/// relative-colorimetric pipeline: the shared evaluator for dE.
fn to_lab(dst: &End, space: Space, units: &[f64]) -> Vec<Lab> {
    if space == Space::Lab {
        return units
            .chunks_exact(3)
            .map(|u| Lab::new(u[0] * 100.0, u[1] * 255.0 - 128.0, u[2] * 255.0 - 128.0))
            .collect();
    }
    let d = dst.lcms();
    let lab = Prof::lab4();
    let x = Xf::new(
        &d,
        space.fmt(Ty::U8) & !7 | (1 << 22), // DBL: FLOAT_SH(1), BYTES_SH(0)
        &lab,
        lcms::TYPE_LAB_DBL,
        1,
        lcms::FLAGS_NOOPTIMIZE | lcms::FLAGS_NOCACHE,
    );
    let src: Vec<f64> = if space == Space::Cmyk {
        units.iter().map(|u| u * 100.0).collect()
    } else {
        units.to_vec()
    };
    let mut out = vec![0f64; src.len() / space.ch() * 3];
    x.run(&src, &mut out);
    out.chunks_exact(3)
        .map(|l| Lab::new(l[0], l[1], l[2]))
        .collect()
}

fn diff_stats(a: &[f64], b: &[f64], scale: f64) -> (f64, f64) {
    let (mut mx, mut sum) = (0.0f64, 0.0);
    for (x, y) in a.iter().zip(b) {
        let d = (x - y).abs() * scale;
        mx = mx.max(d);
        sum += d;
    }
    (mx, sum / a.len().max(1) as f64)
}

fn compare(c: &Case, ty: Ty, exact: &[f64]) -> Row {
    UNBOUNDED.store(0, std::sync::atomic::Ordering::Relaxed);
    let (a, b) = run_case(c, ty);
    let unbounded = UNBOUNDED.load(std::sync::atomic::Ordering::Relaxed);
    let scale = match ty {
        Ty::U8 => 255.0,
        Ty::U16 => 65535.0,
        _ => 1.0,
    };
    let (mx, mean_diff) = diff_stats(&a, &b, scale);
    let la = to_lab(&c.dst, c.dst_space, &a);
    let lb = to_lab(&c.dst, c.dst_space, &b);
    let (mut mde, mut sde) = (0.0f64, 0.0);
    for (x, y) in la.iter().zip(&lb) {
        let d = delta_e_2000(*x, *y);
        mde = mde.max(d);
        sde += d;
    }
    // Each integer engine against lcms2's exact float pipeline (clamped),
    // quantised to the same sample type: who carries the integer error.
    let q = |v: &[f64]| -> Vec<f64> { v.iter().map(|&x| (x * scale).round() / scale).collect() };
    let (ours_exact, lcms_exact) = match ty {
        Ty::U8 | Ty::U16 => {
            let e = q(exact);
            (diff_stats(&a, &e, scale), diff_stats(&b, &e, scale))
        }
        _ => ((f64::NAN, f64::NAN), (f64::NAN, f64::NAN)),
    };
    Row {
        ours_exact,
        lcms_exact,
        name: c.name.clone(),
        ty,
        colours: a.len() / c.dst_space.ch(),
        max_diff: mx,
        mean_diff,
        mean_de: sde / la.len() as f64,
        max_de: mde,
        unbounded,
    }
}

const INTENTS: [Intent; 4] = [
    Intent::Perceptual,
    Intent::RelativeColorimetric,
    Intent::Saturation,
    Intent::AbsoluteColorimetric,
];

fn short(i: Intent) -> &'static str {
    match i {
        Intent::Perceptual => "perc",
        Intent::RelativeColorimetric => "rel",
        Intent::Saturation => "sat",
        Intent::AbsoluteColorimetric => "abs",
    }
}

fn flare_camera() -> Profile {
    let mut p = common::camera_lut16_lab();
    if let Some(Tag::Lut(Lut::Lut16(mut l))) = p.tag(*b"A2B0").cloned() {
        for px in l.clut.data.chunks_exact_mut(3) {
            let lstar = f64::from(px[0]) / 65280.0 * 100.0;
            px[0] = ((6.0 + lstar * 0.94) / 100.0 * 65280.0).round() as u16;
        }
        p.set_tag(*b"A2B0", Tag::Lut(Lut::Lut16(l)));
    }
    p
}

fn cases() -> Vec<Case> {
    let srgb = End::Bytes(Profile::new_srgb().encode());
    let p3 = End::Bytes(Profile::new_display_p3().encode());
    let r2020 = End::Bytes(Profile::new_rec2020().encode());
    let v4pref = End::Bytes(data("sRGB_v4_ICC_preference.icc"));
    let cgats = End::Bytes(data("CGATS001Compat-v2-micro.icc"));
    let srgb_v2 = End::Bytes(data("sRGB-v2-magic.icc"));
    let grey4 = End::Bytes(data("sGrey-v4.icc"));
    let grey2 = End::Bytes(data("sGrey-v2-magic.icc"));
    let cam16 = End::Bytes(common::camera_lut16_lab().encode());
    let cammab = End::Bytes(common::camera_mab_xyz().encode());
    let flare = End::Bytes(flare_camera().encode());
    let mut v = Vec::new();
    let mut add =
        |name: &str, src: &End, ss, dst: &End, ds, proof: Option<(End, Intent)>, i, bpc, n| {
            v.push(Case {
                name: format!("{name} {}{}", short(i), if bpc { " +BPC" } else { "" }),
                src: src.clone(),
                src_space: ss,
                dst: dst.clone(),
                dst_space: ds,
                proof,
                intent: i,
                bpc,
                n,
            });
        };
    use Space::*;
    for i in INTENTS {
        add("sRGB->P3", &srgb, Rgb, &p3, Rgb, None, i, false, 33);
        add("P3->sRGB", &p3, Rgb, &srgb, Rgb, None, i, false, 33);
        add("sRGB->Rec2020", &srgb, Rgb, &r2020, Rgb, None, i, false, 33);
        add("Rec2020->sRGB", &r2020, Rgb, &srgb, Rgb, None, i, false, 33);
        add(
            "sRGBv4pref(mAB)->P3",
            &v4pref,
            Rgb,
            &p3,
            Rgb,
            None,
            i,
            false,
            33,
        );
        add(
            "P3->sRGBv4pref(mBA)",
            &p3,
            Rgb,
            &v4pref,
            Rgb,
            None,
            i,
            false,
            33,
        );
        add(
            "CGATS001(lut16)->sRGB",
            &cgats,
            Cmyk,
            &srgb,
            Rgb,
            None,
            i,
            false,
            9,
        );
        add(
            "camera(lut16 Lab)->sRGB",
            &cam16,
            Rgb,
            &srgb,
            Rgb,
            None,
            i,
            false,
            33,
        );
        add(
            "camera(mAB XYZ)->sRGB",
            &cammab,
            Rgb,
            &srgb,
            Rgb,
            None,
            i,
            false,
            33,
        );
        for bpc in [false, true] {
            add(
                "flare camera->sRGBv2",
                &flare,
                Rgb,
                &srgb_v2,
                Rgb,
                None,
                i,
                bpc,
                17,
            );
        }
    }
    if let Some(f) = fogra() {
        let fogra = End::Bytes(f);
        for i in INTENTS {
            for bpc in [false, true] {
                add("sRGB->FOGRA39", &srgb, Rgb, &fogra, Cmyk, None, i, bpc, 17);
                add("FOGRA39->sRGB", &fogra, Cmyk, &srgb, Rgb, None, i, bpc, 9);
                add(
                    "FOGRA39->sRGBv2",
                    &fogra,
                    Cmyk,
                    &srgb_v2,
                    Rgb,
                    None,
                    i,
                    bpc,
                    9,
                );
            }
        }
        for i in [Intent::Perceptual, Intent::RelativeColorimetric] {
            for pi in [Intent::RelativeColorimetric, Intent::AbsoluteColorimetric] {
                for bpc in [false, true] {
                    add(
                        &format!("proof sRGB->FOGRA39->sRGB (proof {})", short(pi)),
                        &srgb,
                        Rgb,
                        &srgb,
                        Rgb,
                        Some((fogra.clone(), pi)),
                        i,
                        bpc,
                        17,
                    );
                }
            }
        }
    } else {
        eprintln!("RUSTY_ICC_FOGRA39 not set: FOGRA39 cases skipped");
    }
    for i in [Intent::RelativeColorimetric, Intent::AbsoluteColorimetric] {
        add("sRGB->Lab", &srgb, Rgb, &End::Lab, Lab, None, i, false, 33);
        add("Lab->sRGB", &End::Lab, Lab, &srgb, Rgb, None, i, false, 33);
        add(
            "sGrey v4->sRGB",
            &grey4,
            Gray,
            &srgb,
            Rgb,
            None,
            i,
            false,
            256,
        );
        add(
            "sRGB->sGrey v2",
            &srgb,
            Rgb,
            &grey2,
            Gray,
            None,
            i,
            false,
            17,
        );
    }
    v
}

fn accuracy() {
    println!("| case | type | colours | max diff | mean diff | mean dE00 | max dE00 | lcms2 float outside 0..1 | rusty_icc vs exact (max/mean) | lcms2 vs exact (max/mean) |");
    println!("|---|---|---:|---:|---:|---:|---:|---:|---|---|");
    for c in cases() {
        let (_, exact) = run_case(&c, Ty::F32Exact);
        for ty in [Ty::U8, Ty::U16, Ty::F32, Ty::F32Exact] {
            let r = compare(&c, ty, &exact);
            let pair = |p: (f64, f64)| {
                if p.0.is_nan() {
                    "-".to_string()
                } else {
                    format!("{:.0} / {:.3}", p.0, p.1)
                }
            };
            println!(
                "| {} | {:?} | {} | {:.4} | {:.5} | {:.4} | {:.4} | {} | {} | {} |",
                r.name,
                r.ty,
                r.colours,
                r.max_diff,
                r.mean_diff,
                r.mean_de,
                r.max_de,
                r.unbounded,
                pair(r.ours_exact),
                pair(r.lcms_exact)
            );
        }
    }
}

fn blackpoints() {
    let mut profs: Vec<(&str, Vec<u8>)> = vec![
        ("sRGB (rusty_icc v4)", Profile::new_srgb().encode()),
        ("sRGB-v2-magic", data("sRGB-v2-magic.icc")),
        ("sRGB v4 ICC pref", data("sRGB_v4_ICC_preference.icc")),
        ("CGATS001 lut16", data("CGATS001Compat-v2-micro.icc")),
        ("camera lut16", common::camera_lut16_lab().encode()),
        ("flare camera lut16", flare_camera().encode()),
        ("camera mAB", common::camera_mab_xyz().encode()),
    ];
    if let Some(f) = fogra() {
        profs.push(("Coated FOGRA39", f));
    }
    println!("| profile | intent | kind | rusty_icc XYZ | lcms2 XYZ | dE00 |");
    println!("|---|---|---|---|---|---:|");
    let mut worst = 0.0f64;
    for (name, b) in &profs {
        let ours = Profile::new_from_slice(b).unwrap();
        let theirs = Prof::from_mem(b);
        for i in [
            Intent::Perceptual,
            Intent::RelativeColorimetric,
            Intent::Saturation,
        ] {
            for dest in [false, true] {
                let o = if dest {
                    ours.detect_destination_black_point(i)
                } else {
                    ours.detect_black_point(i)
                };
                let t = if dest {
                    theirs.dest_black_point(lcms_intent(i))
                } else {
                    theirs.black_point(lcms_intent(i))
                };
                let de = delta_e_2000(xyz_to_lab(o, D50), xyz_to_lab(Xyz::new(t.x, t.y, t.z), D50));
                worst = worst.max(de);
                println!(
                    "| {name} | {} | {} | ({:.5}, {:.5}, {:.5}) | ({:.5}, {:.5}, {:.5}) | {de:.4} |",
                    short(i),
                    if dest { "destination" } else { "source" },
                    o.x,
                    o.y,
                    o.z,
                    t.x,
                    t.y,
                    t.z
                );
            }
        }
    }
    println!("\nworst black-point dE00: {worst:.4}");
}

// ------------------------------------------------------------------ speed

struct Rng(u64);
impl Rng {
    fn next(&mut self) -> u64 {
        self.0 ^= self.0 >> 12;
        self.0 ^= self.0 << 25;
        self.0 ^= self.0 >> 27;
        self.0.wrapping_mul(0x2545_F491_4F6C_DD1D)
    }
    fn noise(&mut self) -> f64 {
        (self.next() >> 11) as f64 / (1u64 << 53) as f64 - 0.5
    }
}

/// Smooth gradients with a sinusoidal structure plus per-pixel noise, in
/// unit values: real-content-like (many distinct colours, local
/// correlation) and deterministic.
// `6.283` (not TAU) is part of the image definition behind the published
// speed numbers; changing it changes the pixels.
#[allow(clippy::approx_constant, clippy::needless_range_loop)]
fn image(w: usize, h: usize, ch: usize) -> Vec<f64> {
    let mut rng = Rng(0x5EED_1CC0);
    let mut v = Vec::with_capacity(w * h * ch);
    for y in 0..h {
        for x in 0..w {
            let fx = x as f64 / w as f64;
            let fy = y as f64 / h as f64;
            let base = [
                fx,
                fy,
                0.5 + 0.5 * (6.283 * (fx * 3.0 + fy * 2.0)).sin(),
                (fx * fy).sqrt(),
            ];
            for c in 0..ch {
                v.push((base[c] + 0.06 * rng.noise()).clamp(0.0, 1.0));
            }
        }
    }
    v
}

trait Arm {
    fn run(&mut self);
    fn checksum(&self) -> u64;
}

struct Bufs<I, O> {
    src: Vec<I>,
    dst: Vec<O>,
}

fn sum_bytes<T: Copy>(v: &[T]) -> u64 {
    // FNV over the in-memory bytes of the output.
    let n = std::mem::size_of_val(v);
    let mut h = 0xcbf2_9ce4_8422_2325u64;
    let mut b = Vec::with_capacity(n);
    for x in v {
        let p = x as *const T as *const u8;
        // SAFETY: reading the bytes of a Copy value we hold a reference to.
        b.extend_from_slice(unsafe { std::slice::from_raw_parts(p, std::mem::size_of::<T>()) });
    }
    for byte in b {
        h = (h ^ u64::from(byte)).wrapping_mul(0x100_0000_01b3);
    }
    h
}

struct OursArm<T: rusty_icc::Sample> {
    t: Transform,
    b: Bufs<T, T>,
}
impl<T: rusty_icc::Sample> Arm for OursArm<T> {
    fn run(&mut self) {
        self.t.transform(&self.b.src, &mut self.b.dst).unwrap();
    }
    fn checksum(&self) -> u64 {
        sum_bytes(&self.b.dst)
    }
}

struct LcmsArm<I: Copy, O: Copy> {
    x: Xf,
    b: Bufs<I, O>,
}
impl<I: Copy, O: Copy> Arm for LcmsArm<I, O> {
    fn run(&mut self) {
        self.x.run(&self.b.src, &mut self.b.dst);
    }
    fn checksum(&self) -> u64 {
        sum_bytes(&self.b.dst)
    }
}

fn median(v: &mut [f64]) -> f64 {
    v.sort_by(|a, b| a.partial_cmp(b).unwrap());
    let n = v.len();
    if n % 2 == 1 {
        v[n / 2]
    } else {
        0.5 * (v[n / 2 - 1] + v[n / 2])
    }
}

fn speed(case: &str, pairs: usize, side: usize) {
    let fogra_b = fogra();
    let srgb_b = Profile::new_srgb().encode();
    let p3_b = Profile::new_display_p3().encode();
    let (w, h) = (side, side);
    let px = w * h;
    let srgb = End::Bytes(srgb_b);
    let p3 = End::Bytes(p3_b);
    let fog = || End::Bytes(fogra_b.clone().expect("RUSTY_ICC_FOGRA39 required"));
    let (src, ss, dst, ds, ty) = match case {
        "srgb-p3-u8" => (srgb, Space::Rgb, p3, Space::Rgb, Ty::U8),
        "srgb-p3-u16" => (srgb, Space::Rgb, p3, Space::Rgb, Ty::U16),
        "srgb-fogra-u8" => (srgb, Space::Rgb, fog(), Space::Cmyk, Ty::U8),
        "fogra-srgb-u16" => (fog(), Space::Cmyk, srgb, Space::Rgb, Ty::U16),
        "srgb-lab-f32" => (srgb, Space::Rgb, End::Lab, Space::Lab, Ty::F32),
        "lab-srgb-f32" => (End::Lab, Space::Lab, srgb, Space::Rgb, Ty::F32),
        _ => panic!("unknown case {case}"),
    };
    let intent = Intent::RelativeColorimetric;
    let units = image(w, h, ss.ch());
    let (so, do_) = (src.ours(), dst.ours());
    let (sl, dl) = (src.lcms(), dst.lcms());
    let mk_ours = || {
        Transform::new(
            &so,
            ss.layout(),
            &do_,
            ds.layout(),
            intent,
            Options::default(),
        )
        .unwrap()
    };
    let flag_sets = [0u32, lcms::FLAGS_HIGHRESPRECALC | lcms::FLAGS_NOCACHE];
    let mut arms: Vec<(String, Box<dyn Arm>)> = Vec::new();
    match ty {
        Ty::U8 => {
            let s: Vec<u8> = units.iter().map(|&u| (u * 255.0).round() as u8).collect();
            for name in ["rusty_icc", "rusty_icc(null)"] {
                arms.push((
                    name.into(),
                    Box::new(OursArm {
                        t: mk_ours(),
                        b: Bufs {
                            src: s.clone(),
                            dst: vec![0u8; px * ds.ch()],
                        },
                    }),
                ));
            }
            for f in flag_sets {
                let x = Xf::new(&sl, ss.fmt(ty), &dl, ds.fmt(ty), lcms_intent(intent), f);
                arms.push((
                    format!("lcms2 flags=0x{f:x}"),
                    Box::new(LcmsArm {
                        x,
                        b: Bufs {
                            src: s.clone(),
                            dst: vec![0u8; px * ds.ch()],
                        },
                    }),
                ));
            }
        }
        Ty::U16 => {
            let s: Vec<u16> = units
                .iter()
                .map(|&u| (u * 65535.0).round() as u16)
                .collect();
            for name in ["rusty_icc", "rusty_icc(null)"] {
                arms.push((
                    name.into(),
                    Box::new(OursArm {
                        t: mk_ours(),
                        b: Bufs {
                            src: s.clone(),
                            dst: vec![0u16; px * ds.ch()],
                        },
                    }),
                ));
            }
            for f in flag_sets {
                let x = Xf::new(&sl, ss.fmt(ty), &dl, ds.fmt(ty), lcms_intent(intent), f);
                arms.push((
                    format!("lcms2 flags=0x{f:x}"),
                    Box::new(LcmsArm {
                        x,
                        b: Bufs {
                            src: s.clone(),
                            dst: vec![0u16; px * ds.ch()],
                        },
                    }),
                ));
            }
        }
        _ => {
            let sc = ss.ch();
            let so_: Vec<f32> = units
                .iter()
                .enumerate()
                .map(|(i, &u)| unit_to_f32(ss, i % sc, u, false))
                .collect();
            let sl_: Vec<f32> = units
                .iter()
                .enumerate()
                .map(|(i, &u)| unit_to_f32(ss, i % sc, u, true))
                .collect();
            for name in ["rusty_icc", "rusty_icc(null)"] {
                arms.push((
                    name.into(),
                    Box::new(OursArm {
                        t: mk_ours(),
                        b: Bufs {
                            src: so_.clone(),
                            dst: vec![0f32; px * ds.ch()],
                        },
                    }),
                ));
            }
            for f in [0u32, lcms::FLAGS_NOCACHE] {
                let x = Xf::new(&sl, ss.fmt(ty), &dl, ds.fmt(ty), lcms_intent(intent), f);
                arms.push((
                    format!("lcms2 flags=0x{f:x}"),
                    Box::new(LcmsArm {
                        x,
                        b: Bufs {
                            src: sl_.clone(),
                            dst: vec![0f32; px * ds.ch()],
                        },
                    }),
                ));
            }
        }
    }
    // Warm-up (page faults, lazy tables), then interleaved rounds whose
    // order reverses every round (A B C D / D C B A).
    for (_, a) in &mut arms {
        a.run();
    }
    let n_arms = arms.len();
    let mut wall = vec![Vec::new(); n_arms];
    let mut cpu_ratio = vec![Vec::new(); n_arms];
    let mut sums = vec![0u64; n_arms];
    for round in 0..pairs {
        let order: Vec<usize> = if round % 2 == 0 {
            (0..n_arms).collect()
        } else {
            (0..n_arms).rev().collect()
        };
        for &k in &order {
            let c0 = lcms::thread_cpu();
            let t0 = Instant::now();
            arms[k].1.run();
            let dt = t0.elapsed().as_secs_f64();
            let dc = lcms::thread_cpu() - c0;
            wall[k].push(dt);
            cpu_ratio[k].push(dc / dt);
        }
    }
    for (k, (_, a)) in arms.iter().enumerate() {
        sums[k] = a.checksum();
    }
    println!(
        "case {case}: {w}x{h} = {:.2} Mpx, {} pairs, intent relative, single thread",
        px as f64 / 1e6,
        pairs
    );
    println!(
        "| arm | min ms | median ms | Mpx/s (min) | Mpx/s (median) | cpu/wall range | output FNV |"
    );
    println!("|---|---:|---:|---:|---:|---|---|");
    for k in 0..n_arms {
        let mn = wall[k].iter().cloned().fold(f64::MAX, f64::min);
        let md = median(&mut wall[k].clone());
        let (cl, ch) = cpu_ratio[k]
            .iter()
            .fold((f64::MAX, 0.0f64), |(a, b), &v| (a.min(v), b.max(v)));
        println!(
            "| {} | {:.1} | {:.1} | {:.1} | {:.1} | {:.3}-{:.3} | {:016x} |",
            arms[k].0,
            mn * 1e3,
            md * 1e3,
            px as f64 / 1e6 / mn,
            px as f64 / 1e6 / md,
            cl,
            ch,
            sums[k]
        );
    }
    // Paired comparisons of arm 0 (rusty_icc) against every other arm.
    println!("\n| pair | rusty_icc faster in | z | median ratio (other / rusty_icc) | worst-pair ratio |");
    println!("|---|---:|---:|---:|---:|");
    for k in 1..n_arms {
        let ratios: Vec<f64> = wall[k].iter().zip(&wall[0]).map(|(o, r)| o / r).collect();
        let wins = ratios.iter().filter(|&&r| r > 1.0).count();
        let z = (wins as f64 - pairs as f64 / 2.0) / (0.5 * (pairs as f64).sqrt());
        let worst = ratios.iter().cloned().fold(1.0f64, |a, r| {
            if (r - 1.0).abs() > (a - 1.0).abs() {
                r
            } else {
                a
            }
        });
        println!(
            "| rusty_icc vs {} | {wins}/{pairs} | {z:+.2} | {:.3} | {:.3} |",
            arms[k].0,
            median(&mut ratios.clone()),
            worst
        );
    }
}

fn main() {
    let args: Vec<String> = std::env::args().collect();
    match args.get(1).map(String::as_str) {
        Some("accuracy") => accuracy(),
        Some("blackpoints") => blackpoints(),
        Some("speed") => speed(
            &args[2],
            args.get(3).map_or(20, |s| s.parse().unwrap()),
            args.get(4).map_or(4096, |s| s.parse().unwrap()),
        ),
        _ => eprintln!("usage: bench-lcms accuracy | blackpoints | speed CASE [PAIRS] [SIDE]"),
    }
}
