//! Oracle comparison against `moxcms` 0.9 (pure Rust; dev-dependency only).
//!
//! Both engines parse the SAME profile bytes and transform the same dense
//! grid of colours. Per case the table reports the max and mean absolute
//! per-channel difference (in 8-bit codes for `u8`, 16-bit codes for `u16`,
//! and in `0..1` units for `f32`) and the mean/max CIEDE2000 between the two
//! outputs, both interpreted through the destination profile's relative
//! colorimetric device-to-Lab transform (one shared evaluator, so it only
//! measures the output difference).
//!
//! moxcms is configured for its most precise mode: tetrahedral
//! interpolation, floating point (`prefer_fixed_point = false`), no CICP
//! shortcut. Known semantic differences, which the case list avoids or
//! labels: moxcms has no black-point compensation (neither the option nor
//! `LittleCMS`'s implicit v4 perceptual BPC) and implements absolute
//! colorimetric as relative.
//!
//! Run with `cargo test -p rusty_icc --release --test oracle -- --nocapture`
//! to print the table.

mod common;

use common::{camera_lut16_lab, camera_mab_xyz, data, fogra39};
use moxcms::{ColorProfile, InterpolationMethod, Layout as ML, RenderingIntent, TransformOptions};
use rusty_icc::color::{delta_e_2000, Lab};
use rusty_icc::{Intent, Layout, Options, Profile, Transform};

#[derive(Clone, Copy, PartialEq, Debug)]
enum Ty {
    U8,
    U16,
    F32,
}

struct Row {
    name: String,
    ty: Ty,
    n: usize,
    max_diff: f64,
    mean_diff: f64,
    mean_de: f64,
    max_de: f64,
}

fn mox_intent(i: Intent) -> RenderingIntent {
    match i {
        Intent::Perceptual => RenderingIntent::Perceptual,
        Intent::RelativeColorimetric => RenderingIntent::RelativeColorimetric,
        Intent::Saturation => RenderingIntent::Saturation,
        Intent::AbsoluteColorimetric => RenderingIntent::AbsoluteColorimetric,
    }
}

fn mox_layout(l: Layout) -> ML {
    match l {
        Layout::Rgb => ML::Rgb,
        Layout::Rgba | Layout::Cmyk => ML::Rgba,
        Layout::Gray => ML::Gray,
        Layout::GrayAlpha => ML::GrayAlpha,
        _ => unreachable!("layout not used in oracle cases"),
    }
}

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

fn to_lab(dst: &Profile, layout: Layout, units: &[f64]) -> Vec<Lab> {
    let lab = Profile::new_lab_identity();
    let t = Transform::new(
        dst,
        layout,
        &lab,
        Layout::Lab,
        Intent::RelativeColorimetric,
        Options::default(),
    )
    .expect("to-Lab transform");
    let src: Vec<f32> = units.iter().map(|&v| v as f32).collect();
    let mut out = vec![0f32; src.len() / layout.channels() * 3];
    t.transform(&src, &mut out).expect("to-Lab");
    out.chunks_exact(3)
        .map(|c| Lab::new(f64::from(c[0]), f64::from(c[1]), f64::from(c[2])))
        .collect()
}

#[allow(clippy::too_many_arguments)]
fn case(
    name: &str,
    src_bytes: &[u8],
    src_layout: Layout,
    dst_bytes: &[u8],
    dst_layout: Layout,
    intent: Intent,
    ty: Ty,
    n: usize,
) -> Row {
    let src_p = Profile::new_from_slice(src_bytes).expect("parse src");
    let dst_p = Profile::new_from_slice(dst_bytes).expect("parse dst");
    let ours = Transform::new(
        &src_p,
        src_layout,
        &dst_p,
        dst_layout,
        intent,
        Options::default(),
    )
    .expect("rusty_icc transform");
    let ms = ColorProfile::new_from_slice(src_bytes).expect("moxcms parse src");
    let md = ColorProfile::new_from_slice(dst_bytes).expect("moxcms parse dst");
    let opts = TransformOptions {
        rendering_intent: mox_intent(intent),
        allow_use_cicp_transfer: false,
        prefer_fixed_point: false,
        interpolation_method: InterpolationMethod::Tetrahedral,
        ..TransformOptions::default()
    };
    let units = grid(src_layout.channels(), n);
    let pixels = units.len() / src_layout.channels();
    let dc = dst_layout.channels();
    let (a, b, scale): (Vec<f64>, Vec<f64>, f64) = match ty {
        Ty::U8 => {
            let src: Vec<u8> = units.iter().map(|&v| (v * 255.0).round() as u8).collect();
            let mut o = vec![0u8; pixels * dc];
            let mut m = vec![0u8; pixels * dc];
            ours.transform(&src, &mut o).expect("ours");
            ms.create_transform_8bit(mox_layout(src_layout), &md, mox_layout(dst_layout), opts)
                .expect("moxcms 8-bit")
                .transform(&src, &mut m)
                .expect("moxcms run");
            (
                o.iter().map(|&v| f64::from(v) / 255.0).collect(),
                m.iter().map(|&v| f64::from(v) / 255.0).collect(),
                255.0,
            )
        }
        Ty::U16 => {
            let src: Vec<u16> = units
                .iter()
                .map(|&v| (v * 65535.0).round() as u16)
                .collect();
            let mut o = vec![0u16; pixels * dc];
            let mut m = vec![0u16; pixels * dc];
            ours.transform(&src, &mut o).expect("ours");
            ms.create_transform_16bit(mox_layout(src_layout), &md, mox_layout(dst_layout), opts)
                .expect("moxcms 16-bit")
                .transform(&src, &mut m)
                .expect("moxcms run");
            (
                o.iter().map(|&v| f64::from(v) / 65535.0).collect(),
                m.iter().map(|&v| f64::from(v) / 65535.0).collect(),
                65535.0,
            )
        }
        Ty::F32 => {
            let src: Vec<f32> = units.iter().map(|&v| v as f32).collect();
            let mut o = vec![0f32; pixels * dc];
            let mut m = vec![0f32; pixels * dc];
            ours.transform(&src, &mut o).expect("ours");
            ms.create_transform_f32(mox_layout(src_layout), &md, mox_layout(dst_layout), opts)
                .expect("moxcms f32")
                .transform(&src, &mut m)
                .expect("moxcms run");
            (
                o.iter().map(|&v| f64::from(v)).collect(),
                m.iter().map(|&v| f64::from(v).clamp(0.0, 1.0)).collect(),
                1.0,
            )
        }
    };
    let mut max_diff = 0.0f64;
    let mut sum_diff = 0.0;
    for (x, y) in a.iter().zip(&b) {
        let d = (x - y).abs() * scale;
        max_diff = max_diff.max(d);
        sum_diff += d;
    }
    let la = to_lab(&dst_p, dst_layout, &a);
    let lb = to_lab(&dst_p, dst_layout, &b);
    let mut max_de = 0.0f64;
    let mut sum_de = 0.0;
    for (x, y) in la.iter().zip(&lb) {
        let d = delta_e_2000(*x, *y);
        max_de = max_de.max(d);
        sum_de += d;
    }
    Row {
        name: format!("{name} {intent:?}"),
        ty,
        n: pixels,
        max_diff,
        mean_diff: sum_diff / a.len() as f64,
        mean_de: sum_de / la.len() as f64,
        max_de,
    }
}

fn print(rows: &[Row]) {
    println!("| case | type | colours | max diff | mean diff | mean dE00 | max dE00 |");
    println!("|---|---|---:|---:|---:|---:|---:|");
    for r in rows {
        println!(
            "| {} | {:?} | {} | {:.4} | {:.4} | {:.4} | {:.4} |",
            r.name, r.ty, r.n, r.max_diff, r.mean_diff, r.mean_de, r.max_de
        );
    }
}

const ALL: [Intent; 4] = [
    Intent::Perceptual,
    Intent::RelativeColorimetric,
    Intent::Saturation,
    Intent::AbsoluteColorimetric,
];
const TYPES: [Ty; 3] = [Ty::U8, Ty::U16, Ty::F32];

#[test]
fn srgb_display_p3_all_intents() {
    let srgb = Profile::new_srgb().encode();
    let p3 = Profile::new_display_p3().encode();
    let mut rows = Vec::new();
    for ty in TYPES {
        for intent in ALL {
            rows.push(case(
                "sRGB->P3",
                &srgb,
                Layout::Rgb,
                &p3,
                Layout::Rgb,
                intent,
                ty,
                33,
            ));
            rows.push(case(
                "P3->sRGB",
                &p3,
                Layout::Rgb,
                &srgb,
                Layout::Rgb,
                intent,
                ty,
                33,
            ));
        }
    }
    print(&rows);
    for r in &rows {
        check_matrix_shaper(r);
    }
}

#[test]
fn rec2020_and_v2_matrix_profiles() {
    let srgb = Profile::new_srgb().encode();
    let r2020 = Profile::new_rec2020().encode();
    let prophoto = data("ProPhoto-v2-micro.icc");
    let srgb_v2 = data("sRGB-v2-magic.icc");
    let mut rows = Vec::new();
    for ty in TYPES {
        let i = Intent::RelativeColorimetric;
        rows.push(case(
            "sRGB->Rec2020",
            &srgb,
            Layout::Rgb,
            &r2020,
            Layout::Rgb,
            i,
            ty,
            33,
        ));
        rows.push(case(
            "ProPhoto(v2 curv)->sRGB",
            &prophoto,
            Layout::Rgb,
            &srgb,
            Layout::Rgb,
            i,
            ty,
            33,
        ));
        rows.push(case(
            "sRGB(v2 curv)->Rec2020",
            &srgb_v2,
            Layout::Rgb,
            &r2020,
            Layout::Rgb,
            i,
            ty,
            33,
        ));
    }
    print(&rows);
    for r in &rows {
        check_matrix_shaper(r);
    }
}

/// Matrix-shaper to matrix-shaper: at most one code apart in `u8` (whose
/// one-code steps at dark saturated colours are what the u8 max dE
/// measures), a few codes in `u16`, under 1e-3 in `f32`.
fn check_matrix_shaper(r: &Row) {
    let (limit, max_de) = match r.ty {
        Ty::U8 => (1.0 + 1e-9, 2.5),
        Ty::U16 => (20.0, 0.1),
        Ty::F32 => (1e-3, 0.1),
    };
    assert!(r.max_diff <= limit, "{} {:?}: {}", r.name, r.ty, r.max_diff);
    assert!(
        r.mean_de < 0.03,
        "{} {:?}: mean dE {}",
        r.name,
        r.ty,
        r.mean_de
    );
    assert!(r.max_de < max_de, "{} {:?}: dE {}", r.name, r.ty, r.max_de);
}

#[test]
fn v4_mab_lut_profile() {
    // The ICC's sRGB v4 preference profile: mAB/mBA with 17^3 CLUTs, Lab PCS.
    let v4 = data("sRGB_v4_ICC_preference.icc");
    let p3 = Profile::new_display_p3().encode();
    let mut rows = Vec::new();
    for ty in TYPES {
        for intent in [Intent::RelativeColorimetric, Intent::Perceptual] {
            rows.push(case(
                "sRGBv4pref(mAB)->P3",
                &v4,
                Layout::Rgb,
                &p3,
                Layout::Rgb,
                intent,
                ty,
                33,
            ));
            rows.push(case(
                "P3->sRGBv4pref(mBA)",
                &p3,
                Layout::Rgb,
                &v4,
                Layout::Rgb,
                intent,
                ty,
                33,
            ));
        }
    }
    print(&rows);
    for r in &rows {
        // Perceptual differs by design: LittleCMS (and this crate) apply
        // implicit BPC from the v4 perceptual black into a v4 destination;
        // moxcms does not.
        if r.name.contains("Relative") {
            assert!(
                r.mean_de < 0.25 && r.max_de < 2.5,
                "{}: {} {}",
                r.name,
                r.mean_de,
                r.max_de
            );
        }
    }
}

#[test]
fn v2_lut16_cmyk_profiles() {
    // CGATS001 (CC0): v2 scanner-class CMYK with an A2B0 lut16, Lab PCS.
    let cgats = data("CGATS001Compat-v2-micro.icc");
    let srgb = Profile::new_srgb().encode();
    let mut rows = Vec::new();
    for ty in TYPES {
        rows.push(case(
            "CGATS001(lut16 CMYK)->sRGB",
            &cgats,
            Layout::Cmyk,
            &srgb,
            Layout::Rgb,
            Intent::Perceptual,
            ty,
            9,
        ));
    }
    print(&rows);
    for r in &rows {
        // 4-D interpolation differs by design: moxcms is quadlinear, this
        // crate follows LittleCMS (linear in the first input over
        // tetrahedral in the other three); the 6-point grid magnifies it.
        assert!(
            r.mean_de < 0.4 && r.max_de < 4.0,
            "{}: {} {}",
            r.name,
            r.mean_de,
            r.max_de
        );
    }
}

#[test]
fn fogra39_cmyk_both_directions() {
    let Some(fogra) = fogra39() else {
        println!("Coated FOGRA39 not available (set RUSTY_ICC_FOGRA39); skipped");
        return;
    };
    let srgb = Profile::new_srgb().encode();
    let mut rows = Vec::new();
    for ty in TYPES {
        for intent in [
            Intent::Perceptual,
            Intent::RelativeColorimetric,
            Intent::Saturation,
        ] {
            rows.push(case(
                "sRGB->FOGRA39",
                &srgb,
                Layout::Rgb,
                &fogra,
                Layout::Cmyk,
                intent,
                ty,
                17,
            ));
            rows.push(case(
                "FOGRA39->sRGB",
                &fogra,
                Layout::Cmyk,
                &srgb,
                Layout::Rgb,
                intent,
                ty,
                9,
            ));
        }
    }
    print(&rows);
    for r in &rows {
        assert!(
            r.mean_de < 0.25 && r.max_de < 2.0,
            "{}: {} {}",
            r.name,
            r.mean_de,
            r.max_de
        );
    }
}

#[test]
fn camera_style_lut_input_profiles() {
    let srgb = Profile::new_srgb().encode();
    let lut16 = camera_lut16_lab().encode();
    let mab = camera_mab_xyz().encode();
    let mut rows = Vec::new();
    for ty in TYPES {
        rows.push(case(
            "camera(lut16 Lab)->sRGB",
            &lut16,
            Layout::Rgb,
            &srgb,
            Layout::Rgb,
            Intent::Perceptual,
            ty,
            33,
        ));
        rows.push(case(
            "camera(mAB XYZ)->sRGB",
            &mab,
            Layout::Rgb,
            &srgb,
            Layout::Rgb,
            Intent::Perceptual,
            ty,
            33,
        ));
    }
    print(&rows);
    for r in &rows {
        assert!(
            r.mean_de < 0.25 && r.max_de < 2.0,
            "{}: {} {}",
            r.name,
            r.mean_de,
            r.max_de
        );
    }
}

#[test]
fn grey_profiles() {
    let g4 = data("sGrey-v4.icc");
    let g2 = data("sGrey-v2-magic.icc");
    let srgb = Profile::new_srgb().encode();
    let mut rows = Vec::new();
    for ty in TYPES {
        rows.push(case(
            "sGrey v4(para)->sRGB",
            &g4,
            Layout::Gray,
            &srgb,
            Layout::Rgb,
            Intent::RelativeColorimetric,
            ty,
            256,
        ));
        rows.push(case(
            "sGrey v2(curv)->sRGB",
            &g2,
            Layout::Gray,
            &srgb,
            Layout::Rgb,
            Intent::RelativeColorimetric,
            ty,
            256,
        ));
        rows.push(case(
            "sRGB->sGrey v4",
            &srgb,
            Layout::Rgb,
            &g4,
            Layout::Gray,
            Intent::RelativeColorimetric,
            ty,
            17,
        ));
    }
    print(&rows);
    for r in &rows {
        assert!(r.max_de < 1.0, "{}: {}", r.name, r.max_de);
    }
}
