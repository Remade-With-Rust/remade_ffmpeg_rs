//! Fixed workloads for the instruction-count (callgrind Ir) campaign.
//!
//! `icc_ir MODE` runs one workload. The measured work sits in functions
//! named `hot_*` (`#[inline(never)]`), so `valgrind --tool=callgrind
//! --toggle-collect='*hot_*'` counts exactly that and nothing of the setup.
//! Every run prints work-parity anchors: the pixel/byte count and an FNV-1a
//! of the output bytes. If an anchor moves, the experiment changed.
//!
//! `icc_ir MODE REPS` also times the hot call `REPS` more times with a
//! monotonic in-process clock and prints `time_ns min .. median ..` on a
//! second line (the wall check: pin the process and interleave two builds,
//! see `bench-lcms/wall.sh`).
//!
//! Profiles: built-ins, the repository test profiles (`tests/data`), plus
//! Coated FOGRA39 from `RUSTY_ICC_FOGRA39` (v2) and `RUSTY_ICC_FOGRA39_V4`
//! (v4, the `fog4*` modes).

#[global_allocator]
static GLOBAL_ALLOC: rusty_alloc_api::RustyAlloc = rusty_alloc_api::RustyAlloc;

use rusty_icc::color::{Xy, D65_XY};
use rusty_icc::{Curve, GamutCheck, Intent, Layout, Options, Profile, Transform};
use std::hint::black_box;
use std::time::Instant;

const SIDE: usize = 256;
/// Pixels per call of the `small*` workloads: the 256x256 image as 1024
/// separate calls, so per-call setup shows.
const SMALL: usize = 64;

const GREY_V4: &[u8] = include_bytes!("../tests/data/sGrey-v4.icc");
const V4_PREF: &[u8] = include_bytes!("../tests/data/sRGB_v4_ICC_preference.icc");
const PROPHOTO_V2: &[u8] = include_bytes!("../tests/data/ProPhoto-v2-micro.icc");
const CGATS_V2: &[u8] = include_bytes!("../tests/data/CGATS001Compat-v2-micro.icc");

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

/// A v4 Coated FOGRA39 (`lut16` tables, 4096-entry input curves) from
/// `RUSTY_ICC_FOGRA39_V4`: the profile the downstream comparison uses.
fn fogra_v4() -> Profile {
    let path = std::env::var("RUSTY_ICC_FOGRA39_V4").expect("RUSTY_ICC_FOGRA39_V4");
    Profile::new_from_slice(&std::fs::read(path).expect("read FOGRA39 v4")).expect("parse")
}

fn data(bytes: &[u8]) -> Profile {
    Profile::new_from_slice(bytes).expect("parse test profile")
}

/// Rec. 2020 primaries with a linear tone curve.
fn linear_rec2020() -> Profile {
    Profile::new_rgb(
        D65_XY,
        [
            Xy::new(0.708, 0.292),
            Xy::new(0.170, 0.797),
            Xy::new(0.131, 0.046),
        ],
        Curve::gamma(1.0),
    )
    .expect("linear Rec.2020")
}

fn xf(src: &Profile, sl: Layout, dst: &Profile, dl: Layout) -> Transform {
    Transform::new(
        src,
        sl,
        dst,
        dl,
        Intent::RelativeColorimetric,
        Options::default(),
    )
    .unwrap()
}

/// Extra timed repetitions (second argument), 0 under callgrind.
fn reps() -> usize {
    std::env::args()
        .nth(2)
        .and_then(|s| s.parse().ok())
        .unwrap_or(0)
}

/// Times `f` `reps()` times and prints the minimum and median.
fn timed(mut f: impl FnMut()) {
    let n = reps();
    if n == 0 {
        return;
    }
    let mut t: Vec<u128> = (0..n)
        .map(|_| {
            let s = Instant::now();
            f();
            s.elapsed().as_nanos()
        })
        .collect();
    t.sort_unstable();
    println!("time_ns min {} median {} reps {}", t[0], t[n / 2], n);
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
fn hot_small_u8(t: &Transform, src: &[u8], dst: &mut [u8], sc: usize, dc: usize) {
    for (s, d) in src
        .chunks_exact(SMALL * sc)
        .zip(dst.chunks_exact_mut(SMALL * dc))
    {
        t.transform(black_box(s), black_box(d)).unwrap();
    }
}

#[inline(never)]
fn hot_small_u16(t: &Transform, src: &[u16], dst: &mut [u16], sc: usize, dc: usize) {
    for (s, d) in src
        .chunks_exact(SMALL * sc)
        .zip(dst.chunks_exact_mut(SMALL * dc))
    {
        t.transform(black_box(s), black_box(d)).unwrap();
    }
}

#[inline(never)]
fn hot_small_f32(t: &Transform, src: &[f32], dst: &mut [f32], sc: usize, dc: usize) {
    for (s, d) in src
        .chunks_exact(SMALL * sc)
        .zip(dst.chunks_exact_mut(SMALL * dc))
    {
        t.transform(black_box(s), black_box(d)).unwrap();
    }
}

#[inline(never)]
fn hot_build(src: &Profile, sl: Layout, dst: &Profile, dl: Layout) -> Transform {
    xf(black_box(src), sl, black_box(dst), dl)
}

#[inline(never)]
fn hot_build_opts(
    src: &Profile,
    sl: Layout,
    dst: &Profile,
    dl: Layout,
    intent: Intent,
    bpc: bool,
) -> Transform {
    Transform::new(
        black_box(src),
        sl,
        black_box(dst),
        dl,
        intent,
        Options {
            black_point_compensation: bpc,
        },
    )
    .unwrap()
}

#[inline(never)]
fn hot_build_proof(src: &Profile, proof: &Profile, dst: &Profile) -> Transform {
    proofing(black_box(src), black_box(proof), black_box(dst))
}

/// `Transform::new` plus the first 16-bit call (64 pixels): the lazily built
/// 16-bit input tables are part of what a small 16-bit job pays.
#[inline(never)]
fn hot_first16(src: &Profile, dst: &Profile, s: &[u16], o: &mut [u16]) {
    let t = xf(black_box(src), Layout::Rgb, black_box(dst), Layout::Rgb);
    t.transform(black_box(s), black_box(o)).unwrap();
}

#[inline(never)]
fn hot_parse_encode(bytes: &[u8]) -> Vec<u8> {
    Profile::new_from_slice(black_box(bytes)).unwrap().encode()
}

#[inline(never)]
fn hot_gamut_u8(g: &GamutCheck, src: &[u8], out: &mut [f64]) {
    g.delta_e(black_box(src), black_box(out)).unwrap();
}

fn proofing(src: &Profile, proof: &Profile, dst: &Profile) -> Transform {
    proofing_intent(src, proof, dst, Intent::RelativeColorimetric)
}

/// A proof with `intent` into the proof device (relative colorimetric out).
fn proofing_intent(src: &Profile, proof: &Profile, dst: &Profile, intent: Intent) -> Transform {
    Transform::new_proofing(
        src,
        Layout::Rgb,
        proof,
        dst,
        Layout::Rgb,
        intent,
        Intent::RelativeColorimetric,
        Options::default(),
    )
    .unwrap()
}

#[inline(never)]
fn hot_build_proof_intent(src: &Profile, proof: &Profile, dst: &Profile, i: Intent) -> Transform {
    proofing_intent(black_box(src), black_box(proof), black_box(dst), i)
}

fn src_u8(ch: usize) -> Vec<u8> {
    image(ch)
        .iter()
        .map(|&u| (u * 255.0).round() as u8)
        .collect()
}

fn src_u16(ch: usize) -> Vec<u16> {
    image(ch)
        .iter()
        .map(|&u| (u * 65535.0).round() as u16)
        .collect()
}

fn src_f32(ch: usize, lab_in: bool) -> Vec<f32> {
    let mut s: Vec<f32> = image(ch).iter().map(|&u| u as f32).collect();
    if lab_in {
        for px in s.chunks_exact_mut(3) {
            px[0] *= 100.0;
            px[1] = px[1] * 255.0 - 128.0;
            px[2] = px[2] * 255.0 - 128.0;
        }
    }
    s
}

fn anchor_u8(o: &[u8]) {
    println!("pixels {} fnv {:016x}", SIDE * SIDE, fnv(o.iter().copied()));
}

fn anchor_u16(o: &[u16]) {
    println!(
        "pixels {} fnv {:016x}",
        SIDE * SIDE,
        fnv(o.iter().flat_map(|v| v.to_le_bytes()))
    );
}

fn anchor_f32(o: &[f32]) {
    println!(
        "pixels {} fnv {:016x}",
        SIDE * SIDE,
        fnv(o.iter().flat_map(|v| v.to_le_bytes()))
    );
}

fn tr_u8(t: &Transform) {
    let (sl, dl) = (t.src_layout(), t.dst_layout());
    let s = src_u8(sl.channels());
    let mut o = vec![0u8; SIDE * SIDE * dl.channels()];
    t.transform(&s[..sl.channels()], &mut o[..dl.channels()])
        .unwrap(); // warm lazy state
    hot_transform_u8(t, &s, &mut o);
    anchor_u8(&o);
    timed(|| hot_transform_u8(t, &s, &mut o));
}

fn run_u8(src: &Profile, sl: Layout, dst: &Profile, dl: Layout) {
    tr_u8(&xf(src, sl, dst, dl));
}

fn tr_u16(t: &Transform) {
    let (sl, dl) = (t.src_layout(), t.dst_layout());
    let s = src_u16(sl.channels());
    let mut o = vec![0u16; SIDE * SIDE * dl.channels()];
    t.transform(&s[..sl.channels()], &mut o[..dl.channels()])
        .unwrap();
    hot_transform_u16(t, &s, &mut o);
    anchor_u16(&o);
    timed(|| hot_transform_u16(t, &s, &mut o));
}

fn run_u16(src: &Profile, sl: Layout, dst: &Profile, dl: Layout) {
    tr_u16(&xf(src, sl, dst, dl));
}

/// The image as 8-bit values widened to `f32` (`v as f32 / 255.0`): how
/// float pixels decoded from 8-bit files arrive (the downstream comparison's
/// input).
fn src_f32_from_u8(ch: usize) -> Vec<f32> {
    src_u8(ch).iter().map(|&v| f32::from(v) / 255.0).collect()
}

/// [`tr_f32`] on [`src_f32_from_u8`] input.
fn tr_f32_from_u8(t: &Transform) {
    let (sl, dl) = (t.src_layout(), t.dst_layout());
    let s = src_f32_from_u8(sl.channels());
    let mut o = vec![0f32; SIDE * SIDE * dl.channels()];
    hot_transform_f32(t, &s, &mut o);
    anchor_f32(&o);
    timed(|| hot_transform_f32(t, &s, &mut o));
}

fn tr_f32(t: &Transform, lab_in: bool) {
    let (sl, dl) = (t.src_layout(), t.dst_layout());
    let s = src_f32(sl.channels(), lab_in);
    let mut o = vec![0f32; SIDE * SIDE * dl.channels()];
    hot_transform_f32(t, &s, &mut o);
    anchor_f32(&o);
    timed(|| hot_transform_f32(t, &s, &mut o));
}

fn run_f32(src: &Profile, sl: Layout, dst: &Profile, dl: Layout, lab_in: bool) {
    tr_f32(&xf(src, sl, dst, dl), lab_in);
}

fn small_u8(src: &Profile, sl: Layout, dst: &Profile, dl: Layout) {
    let t = xf(src, sl, dst, dl);
    let (sc, dc) = (sl.channels(), dl.channels());
    let s = src_u8(sc);
    let mut o = vec![0u8; SIDE * SIDE * dc];
    t.transform(&s[..sc], &mut o[..dc]).unwrap();
    hot_small_u8(&t, &s, &mut o, sc, dc);
    anchor_u8(&o);
    timed(|| hot_small_u8(&t, &s, &mut o, sc, dc));
}

fn small_u16(src: &Profile, sl: Layout, dst: &Profile, dl: Layout) {
    let t = xf(src, sl, dst, dl);
    let (sc, dc) = (sl.channels(), dl.channels());
    let s = src_u16(sc);
    let mut o = vec![0u16; SIDE * SIDE * dc];
    t.transform(&s[..sc], &mut o[..dc]).unwrap();
    hot_small_u16(&t, &s, &mut o, sc, dc);
    anchor_u16(&o);
    timed(|| hot_small_u16(&t, &s, &mut o, sc, dc));
}

fn small_f32(src: &Profile, sl: Layout, dst: &Profile, dl: Layout) {
    let t = xf(src, sl, dst, dl);
    let (sc, dc) = (sl.channels(), dl.channels());
    let s = src_f32(sc, false);
    let mut o = vec![0f32; SIDE * SIDE * dc];
    hot_small_f32(&t, &s, &mut o, sc, dc);
    anchor_f32(&o);
    timed(|| hot_small_f32(&t, &s, &mut o, sc, dc));
}

/// Anchor of a built transform: its output on a fixed 64-pixel ramp.
fn build_anchor(t: &Transform) {
    let (n, m) = (
        t.src_layout().color_channels(),
        t.dst_layout().color_channels(),
    );
    let s: Vec<u16> = (0..n * 64).map(|i| (i * 1031 % 65536) as u16).collect();
    let mut o = vec![0u16; 64 * m];
    t.transform(&s, &mut o).unwrap();
    println!(
        "pixels 64 fnv {:016x}",
        fnv(o.iter().flat_map(|v| v.to_le_bytes()))
    );
}

fn build(src: &Profile, sl: Layout, dst: &Profile, dl: Layout) {
    build_anchor(&hot_build(src, sl, dst, dl));
    timed(|| {
        black_box(hot_build(src, sl, dst, dl));
    });
}

fn build_opts(src: &Profile, sl: Layout, dst: &Profile, dl: Layout, intent: Intent, bpc: bool) {
    build_anchor(&hot_build_opts(src, sl, dst, dl, intent, bpc));
    timed(|| {
        black_box(hot_build_opts(src, sl, dst, dl, intent, bpc));
    });
}

fn parse(bytes: &[u8]) {
    let out = hot_parse_encode(bytes);
    println!("bytes {} fnv {:016x}", out.len(), fnv(out.iter().copied()));
    timed(|| {
        black_box(hot_parse_encode(bytes));
    });
}

const MODES: &str = "p3u8 p3u16 fogu8 fog16 labf32 labinf32 build3 build4 parse \
p3f32 lin2020f32 fogf32 fogrgbf32 v4f32 v4inf32 prof32 toprof32 prooff32 \
grayu8 togray16 prou8 v4u8 cgatsu8 labu8 lab16in proofu8 gamutu8 \
smallp3u8 smallfogu8 smallfog16 smallp3f32 \
buildp3 buildgray buildlab buildv4 buildproof buildbpc first16 parsev4 fogpf32 fogrgbpf32 proofpf32 build4p buildproofp fogpf32q fog4pf32 fog4pf32q p3f32q lin2020f32q fog4rgbf32q";

#[allow(clippy::too_many_lines)]
fn main() {
    let mode = std::env::args().nth(1).unwrap_or_default();
    let srgb = Profile::new_srgb();
    let p3 = Profile::new_display_p3();
    let lab = Profile::new_lab_identity();
    let (rgb, cmyk, gray, labl) = (Layout::Rgb, Layout::Cmyk, Layout::Gray, Layout::Lab);
    match mode.as_str() {
        // The nine workloads of the 0.1.0 -> 0.1.1 pass.
        "p3u8" => run_u8(&srgb, rgb, &p3, rgb),
        "p3u16" => run_u16(&srgb, rgb, &p3, rgb),
        "fogu8" => run_u8(&srgb, rgb, &fogra(), cmyk),
        "fog16" => run_u16(&fogra(), cmyk, &srgb, rgb),
        "labf32" => run_f32(&srgb, rgb, &lab, labl, false),
        "labinf32" => run_f32(&lab, labl, &srgb, rgb, true),
        "build3" => build(&srgb, rgb, &fogra(), cmyk),
        "build4" => build(&fogra(), cmyk, &srgb, rgb),
        "parse" => {
            let path = std::env::var("RUSTY_ICC_FOGRA39").expect("RUSTY_ICC_FOGRA39");
            parse(&std::fs::read(path).unwrap());
        }
        // f32, exact pipeline.
        "p3f32" => run_f32(&srgb, rgb, &p3, rgb, false),
        "lin2020f32" => run_f32(&srgb, rgb, &linear_rec2020(), rgb, false),
        "fogf32" => run_f32(&srgb, rgb, &fogra(), cmyk, false),
        "fogrgbf32" => run_f32(&fogra(), cmyk, &srgb, rgb, false),
        "v4f32" => run_f32(&srgb, rgb, &data(V4_PREF), rgb, false),
        "v4inf32" => run_f32(&data(V4_PREF), rgb, &srgb, rgb, false),
        "prof32" => run_f32(&data(PROPHOTO_V2), rgb, &srgb, rgb, false),
        "toprof32" => run_f32(&srgb, rgb, &data(PROPHOTO_V2), rgb, false),
        "prooff32" => tr_f32(&proofing(&srgb, &fogra(), &srgb), false),
        // Integer: matrix-shaper (grey, sampled curves) and grids.
        "grayu8" => run_u8(&data(GREY_V4), gray, &srgb, rgb),
        "togray16" => run_u16(&srgb, rgb, &data(GREY_V4), gray),
        "prou8" => run_u8(&data(PROPHOTO_V2), rgb, &srgb, rgb),
        "v4u8" => run_u8(&srgb, rgb, &data(V4_PREF), rgb),
        "cgatsu8" => run_u8(&data(CGATS_V2), cmyk, &srgb, rgb),
        "labu8" => run_u8(&srgb, rgb, &lab, labl),
        "lab16in" => run_u16(&lab, labl, &srgb, rgb),
        "proofu8" => tr_u8(&proofing(&srgb, &fogra(), &srgb)),
        "gamutu8" => {
            let g = GamutCheck::new(
                &srgb,
                rgb,
                &fogra(),
                Intent::RelativeColorimetric,
                Options::default(),
                2.0,
            )
            .unwrap();
            let s = src_u8(3);
            let mut o = vec![0f64; SIDE * SIDE];
            hot_gamut_u8(&g, &s, &mut o);
            println!(
                "pixels {} fnv {:016x}",
                SIDE * SIDE,
                fnv(o.iter().flat_map(|v| v.to_le_bytes()))
            );
            timed(|| hot_gamut_u8(&g, &s, &mut o));
        }
        // The same image as 1024 calls of 64 pixels.
        "smallp3u8" => small_u8(&srgb, rgb, &p3, rgb),
        "smallfogu8" => small_u8(&srgb, rgb, &fogra(), cmyk),
        "smallfog16" => small_u16(&fogra(), cmyk, &srgb, rgb),
        "smallp3f32" => small_f32(&srgb, rgb, &p3, rgb),
        // Transform::new.
        "buildp3" => build(&srgb, rgb, &p3, rgb),
        "buildgray" => build(&data(GREY_V4), gray, &srgb, rgb),
        "buildlab" => build(&srgb, rgb, &lab, labl),
        "buildv4" => build(&srgb, rgb, &data(V4_PREF), rgb),
        "buildproof" => {
            let f = fogra();
            build_anchor(&hot_build_proof(&srgb, &f, &srgb));
            timed(|| {
                black_box(hot_build_proof(&srgb, &f, &srgb));
            });
        }
        "buildbpc" => build_opts(&srgb, rgb, &fogra(), cmyk, Intent::Perceptual, true),
        "first16" => {
            let s: Vec<u16> = (0..3 * SMALL).map(|i| (i * 1031 % 65536) as u16).collect();
            let mut o = vec![0u16; 3 * SMALL];
            hot_first16(&srgb, &p3, &s, &mut o);
            println!(
                "pixels {SMALL} fnv {:016x}",
                fnv(o.iter().flat_map(|v| v.to_le_bytes()))
            );
            timed(|| hot_first16(&srgb, &p3, &s, &mut o));
        }
        "parsev4" => parse(V4_PREF),
        // Perceptual twins of the float CMYK, proof and build rows (the
        // downstream comparison runs sRGB -> FOGRA39 perceptual).
        "fogpf32" => {
            let p = Intent::Perceptual;
            let t = Transform::new(&srgb, rgb, &fogra(), cmyk, p, Options::default()).unwrap();
            tr_f32(&t, false);
        }
        "fogrgbpf32" => {
            let p = Intent::Perceptual;
            let t = Transform::new(&fogra(), cmyk, &srgb, rgb, p, Options::default()).unwrap();
            tr_f32(&t, false);
        }
        "proofpf32" => tr_f32(
            &proofing_intent(&srgb, &fogra(), &srgb, Intent::Perceptual),
            false,
        ),
        "build4p" => build_opts(&fogra(), cmyk, &srgb, rgb, Intent::Perceptual, false),
        "buildproofp" => {
            let f = fogra();
            let p = Intent::Perceptual;
            build_anchor(&hot_build_proof_intent(&srgb, &f, &srgb, p));
            timed(|| {
                black_box(hot_build_proof_intent(&srgb, &f, &srgb, p));
            });
        }
        // Float rows on 8-bit-derived input (`*q`) and on the v4 FOGRA39
        // (`fog4*`): the downstream comparison's shapes.
        "fogpf32q" => {
            let p = Intent::Perceptual;
            let t = Transform::new(&srgb, rgb, &fogra(), cmyk, p, Options::default()).unwrap();
            tr_f32_from_u8(&t);
        }
        "fog4pf32" | "fog4pf32q" => {
            let p = Intent::Perceptual;
            let t = Transform::new(&srgb, rgb, &fogra_v4(), cmyk, p, Options::default()).unwrap();
            if mode.ends_with('q') {
                tr_f32_from_u8(&t);
            } else {
                tr_f32(&t, false);
            }
        }
        "fog4rgbf32q" => {
            let p = Intent::Perceptual;
            let t = Transform::new(&fogra_v4(), cmyk, &srgb, rgb, p, Options::default()).unwrap();
            tr_f32_from_u8(&t);
        }
        "p3f32q" => tr_f32_from_u8(&xf(&srgb, rgb, &p3, rgb)),
        "lin2020f32q" => tr_f32_from_u8(&xf(&srgb, rgb, &linear_rec2020(), rgb)),
        _ => eprintln!("modes: {MODES}"),
    }
}
