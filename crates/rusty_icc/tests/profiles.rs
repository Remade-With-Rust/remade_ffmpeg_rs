//! Parsing, round-tripping, built-in profiles and transform behaviour that
//! needs no oracle: self-consistency between the LUT and matrix-shaper
//! paths, the integer fast paths against the exact `f32` pipeline, alpha,
//! L*a*b*, intents, black-point compensation, proofing and gamut checks.

mod common;

use common::{camera_lut16_lab, camera_mab_xyz, data, fogra39, DATA_PROFILES};
use rusty_icc::color::{delta_e_2000, Lab, Xy, Xyz};
use rusty_icc::{
    AbLut, Curve, GamutCheck, Intent, Layout, Lut, Options, Profile, ProfileClass, Tag, Transform,
};

fn rt(p: &Profile) -> Profile {
    let bytes = p.encode();
    let q = Profile::new_from_slice(&bytes).expect("re-parse");
    assert_eq!(&q, p, "encode -> parse changed the profile");
    assert_eq!(q.encode(), bytes, "encoding is not idempotent");
    q
}

#[test]
fn every_test_profile_parses_and_round_trips() {
    let mut all: Vec<(String, Vec<u8>)> = DATA_PROFILES
        .iter()
        .map(|n| ((*n).to_string(), data(n)))
        .collect();
    if let Some(f) = fogra39() {
        all.push(("CoatedFOGRA39.icc".into(), f));
    }
    for (name, bytes) in &all {
        let p = Profile::new_from_slice(bytes).unwrap_or_else(|e| panic!("{name}: {e}"));
        rt(&p);
        assert!(p.description().is_some(), "{name} has a description");
    }
}

#[test]
fn text_tags_decode() {
    let p = Profile::new_from_slice(&data("sRGB_v4_ICC_preference.icc")).unwrap();
    assert!(
        p.description().unwrap().contains("sRGB"),
        "{:?}",
        p.description()
    );
    assert!(p
        .copyright()
        .unwrap()
        .contains("International Color Consortium"));
    let v2 = Profile::new_from_slice(&data("sRGB-v2-magic.icc")).unwrap();
    assert_eq!(v2.copyright().as_deref(), Some("CC0"));
    assert_eq!(v2.version(), (2, 1));
    let lab = Profile::new_from_slice(&data("LAB.icc")).unwrap();
    assert_eq!(lab.copyright().as_deref(), Some("No copyright, use freely"));
}

#[test]
fn builtins_round_trip_and_carry_d65() {
    for p in [
        Profile::new_srgb(),
        Profile::new_display_p3(),
        Profile::new_rec2020(),
        Profile::new_lab_identity(),
        Profile::new_gray(rusty_icc::color::D65_XY, Curve::gamma(2.2)),
        Profile::new_rgb(
            Xy::new(0.3457, 0.3585),
            [
                Xy::new(0.7347, 0.2653),
                Xy::new(0.1596, 0.8404),
                Xy::new(0.0366, 0.0001),
            ],
            Curve::gamma(1.8),
        )
        .unwrap(),
        camera_lut16_lab(),
        camera_mab_xyz(),
    ] {
        let q = rt(&p);
        assert_eq!(q.version(), p.version());
    }
    // chad^-1 * wtpt recovers the D65 white of a v4 display profile.
    let w = Profile::new_srgb().native_white_point().unwrap();
    assert!(
        (w.x - 0.9505).abs() < 1e-3 && (w.z - 1.089).abs() < 2e-3,
        "{w:?}"
    );
    // The encoded v4 profile carries an MD5 profile ID.
    let bytes = Profile::new_srgb().encode();
    assert_ne!(&bytes[84..100], &[0u8; 16]);
}

#[test]
fn builtin_srgb_matches_published_compact_srgb_v4() {
    // saucecontrol's compact sRGB v4 (CC0) vs the built-in: same primaries,
    // same curve, so the colorants agree to s15Fixed16 precision.
    let theirs = Profile::new_from_slice(&data("sRGB-v4.icc")).unwrap();
    let ours = Profile::new_srgb();
    for (a, b) in theirs
        .colorants()
        .unwrap()
        .iter()
        .zip(ours.colorants().unwrap())
    {
        assert!((a.x - b.x).abs() < 2e-4 && (a.y - b.y).abs() < 2e-4 && (a.z - b.z).abs() < 2e-4);
    }
}

fn grid_f32(ch: usize, n: usize) -> Vec<f32> {
    let total = n.pow(ch as u32);
    let mut v = Vec::new();
    for idx in 0..total {
        let mut rem = idx;
        let mut px = vec![0f32; ch];
        for c in (0..ch).rev() {
            px[c] = (rem % n) as f32 / (n - 1) as f32;
            rem /= n;
        }
        v.extend(px);
    }
    v
}

fn run_f32(t: &Transform, src: &[f32]) -> Vec<f32> {
    let mut o = vec![0f32; src.len() / t.src_layout().channels() * t.dst_layout().channels()];
    t.transform(src, &mut o).unwrap();
    o
}

fn max_abs(a: &[f32], b: &[f32]) -> f32 {
    a.iter()
        .zip(b)
        .map(|(x, y)| (x - y).abs())
        .fold(0.0, f32::max)
}

#[test]
fn written_profiles_transform_like_builtins() {
    let rel = Intent::RelativeColorimetric;
    let o = Options::default();
    let a = Transform::new(
        &Profile::new_srgb(),
        Layout::Rgb,
        &Profile::new_display_p3(),
        Layout::Rgb,
        rel,
        o,
    )
    .unwrap();
    let srgb = Profile::new_from_slice(&Profile::new_srgb().encode()).unwrap();
    let p3 = Profile::new_from_slice(&Profile::new_display_p3().encode()).unwrap();
    let b = Transform::new(&srgb, Layout::Rgb, &p3, Layout::Rgb, rel, o).unwrap();
    let src: Vec<u8> = (0..=255u8).flat_map(|v| [v, 255 - v, v / 2]).collect();
    let (mut x, mut y) = (vec![0u8; src.len()], vec![0u8; src.len()]);
    a.transform(&src, &mut x).unwrap();
    b.transform(&src, &mut y).unwrap();
    assert_eq!(x, y);
}

/// sRGB written as an `mAB ` (M curves = sRGB curve, matrix = colorants,
/// XYZ PCS): through the LUT path it must equal the matrix-shaper path.
fn srgb_as_mab() -> Profile {
    let base = Profile::new_srgb();
    let c = base.colorants().unwrap();
    let q = |v: f64| (v * 32768.0 / 65535.0 * 65536.0).round() / 65536.0;
    let m = [
        q(c[0].x),
        q(c[1].x),
        q(c[2].x),
        q(c[0].y),
        q(c[1].y),
        q(c[2].y),
        q(c[0].z),
        q(c[1].z),
        q(c[2].z),
        0.0,
        0.0,
        0.0,
    ];
    let mut p = base;
    for s in [*b"rXYZ", *b"gXYZ", *b"bXYZ", *b"rTRC", *b"gTRC", *b"bTRC"] {
        p.remove_tag(s);
    }
    p.header.class = ProfileClass::ColorSpace;
    p.set_tag(
        *b"A2B0",
        Tag::Lut(Lut::AToB(AbLut {
            inputs: 3,
            outputs: 3,
            a: None,
            clut: None,
            m: Some(vec![Curve::srgb(), Curve::srgb(), Curve::srgb()]),
            matrix: Some(m),
            b: vec![Curve::identity(), Curve::identity(), Curve::identity()],
        })),
    );
    p
}

#[test]
fn lut_path_equals_matrix_path() {
    let p3 = Profile::new_display_p3();
    let rel = Intent::RelativeColorimetric;
    let lut = Transform::new(
        &rt(&srgb_as_mab()),
        Layout::Rgb,
        &p3,
        Layout::Rgb,
        rel,
        Options::default(),
    )
    .unwrap();
    let mat = Transform::new(
        &Profile::new_srgb(),
        Layout::Rgb,
        &p3,
        Layout::Rgb,
        rel,
        Options::default(),
    )
    .unwrap();
    assert!(!lut.is_matrix_shaper() && mat.is_matrix_shaper());
    let src = grid_f32(3, 33);
    let d = max_abs(&run_f32(&lut, &src), &run_f32(&mat, &src));
    println!("LUT path vs matrix path, f32, 33^3: max |diff| = {d:e}");
    assert!(d < 1e-3, "{d}");
    // And the u16 grid engine against the u16 matrix-shaper engine.
    let s16: Vec<u16> = src.iter().map(|&v| (v * 65535.0).round() as u16).collect();
    let (mut a, mut b) = (vec![0u16; s16.len()], vec![0u16; s16.len()]);
    lut.transform(&s16, &mut a).unwrap();
    mat.transform(&s16, &mut b).unwrap();
    let d16 = a.iter().zip(&b).map(|(x, y)| x.abs_diff(*y)).max().unwrap();
    println!("LUT (33^3 grid) vs matrix-shaper, u16: max diff = {d16} codes");
    assert!(d16 <= 80, "{d16}");
}

#[test]
fn integer_engines_track_the_exact_pipeline() {
    let rel = Intent::RelativeColorimetric;
    let cases: Vec<(&str, Profile, Profile)> = vec![
        (
            "sRGB->P3 (shaper)",
            Profile::new_srgb(),
            Profile::new_display_p3(),
        ),
        (
            "P3->Rec2020 (shaper)",
            Profile::new_display_p3(),
            Profile::new_rec2020(),
        ),
        (
            "ProPhoto v2 curv->sRGB (shaper)",
            Profile::new_from_slice(&data("ProPhoto-v2-micro.icc")).unwrap(),
            Profile::new_srgb(),
        ),
        (
            "sRGBv4pref->P3 (grid)",
            Profile::new_from_slice(&data("sRGB_v4_ICC_preference.icc")).unwrap(),
            Profile::new_display_p3(),
        ),
    ];
    for (name, s, d) in cases {
        let t = Transform::new(&s, Layout::Rgb, &d, Layout::Rgb, rel, Options::default()).unwrap();
        let src = grid_f32(3, 52); // 0, 5, 10 ... 255 in 8 bits
        let exact = run_f32(&t, &src);
        let s8: Vec<u8> = src.iter().map(|&v| (v * 255.0).round() as u8).collect();
        let mut o8 = vec![0u8; s8.len()];
        t.transform(&s8, &mut o8).unwrap();
        let d8 = o8
            .iter()
            .zip(&exact)
            .map(|(&o, &e)| (f32::from(o) - e * 255.0).abs())
            .fold(0.0, f32::max);
        let s16: Vec<u16> = src.iter().map(|&v| (v * 65535.0).round() as u16).collect();
        let mut o16 = vec![0u16; s16.len()];
        t.transform(&s16, &mut o16).unwrap();
        let d16 = o16
            .iter()
            .zip(&exact)
            .map(|(&o, &e)| (f32::from(o) - e * 65535.0).abs())
            .fold(0.0, f32::max);
        println!("{name}: u8 max |int - exact| = {d8:.3} codes, u16 = {d16:.2} codes");
        if t.is_matrix_shaper() {
            // u16 worst case is P3->Rec2020 at the BT.709 curve's knee: its
            // s15Fixed16-rounded parameters leave a 0.0007 step in the
            // inverse that the 16384-entry output table straddles.
            assert!(d8 <= 0.51 && d16 <= 12.0, "{name}: {d8} {d16}");
        } else {
            assert!(d8 <= 1.5 && d16 <= 64.0, "{name}: {d8} {d16}");
        }
    }
}

#[test]
fn alpha_is_passed_through_or_made_opaque() {
    let t = Transform::new(
        &Profile::new_srgb(),
        Layout::Rgba,
        &Profile::new_display_p3(),
        Layout::Rgba,
        Intent::Perceptual,
        Options::default(),
    )
    .unwrap();
    let mut o = [0u16; 8];
    t.transform(&[0u16, 0, 0, 1234, 65535, 65535, 65535, 7], &mut o)
        .unwrap();
    assert_eq!((o[3], o[7]), (1234, 7));
    let t = Transform::new(
        &Profile::new_srgb(),
        Layout::Rgb,
        &Profile::new_display_p3(),
        Layout::Rgba,
        Intent::Perceptual,
        Options::default(),
    )
    .unwrap();
    let mut o = [0f32; 4];
    t.transform(&[0.2f32, 0.4, 0.6], &mut o).unwrap();
    assert_eq!(o[3], 1.0);
    let g = Profile::new_from_slice(&data("sGrey-v4.icc")).unwrap();
    let t = Transform::new(
        &Profile::new_srgb(),
        Layout::Rgba,
        &g,
        Layout::GrayAlpha,
        Intent::Perceptual,
        Options::default(),
    )
    .unwrap();
    let mut o = [0u8; 2];
    t.transform(&[128u8, 128, 128, 9], &mut o).unwrap();
    assert_eq!(o, [128, 9]);
    // Buffer mismatches are errors, not panics.
    assert!(t.transform(&[1u8, 2, 3], &mut [0u8; 2]).is_err());
    assert!(t.transform(&[1u8, 2, 3, 4], &mut [0u8; 3]).is_err());
}

#[test]
fn lab_layouts() {
    let srgb = Profile::new_srgb();
    let lab = Profile::new_lab_identity();
    let rel = Intent::RelativeColorimetric;
    let t = Transform::new(
        &srgb,
        Layout::Rgb,
        &lab,
        Layout::Lab,
        rel,
        Options::default(),
    )
    .unwrap();
    let mut f = [0f32; 6];
    t.transform(&[1.0f32, 1.0, 1.0, 0.0, 0.0, 0.0], &mut f)
        .unwrap();
    assert!(
        (f[0] - 100.0).abs() < 0.01 && f[1].abs() < 0.01 && f[2].abs() < 0.01,
        "{f:?}"
    );
    assert!(f[3].abs() < 0.01, "{f:?}");
    let mut b = [0u8; 3];
    t.transform(&[255u8, 255, 255], &mut b).unwrap();
    assert_eq!(b, [255, 128, 128]);
    let mut w = [0u16; 3];
    t.transform(&[65535u16, 65535, 65535], &mut w).unwrap();
    assert!(
        w[0] >= 65530 && w[1].abs_diff(32896) <= 2 && w[2].abs_diff(32896) <= 2,
        "{w:?}"
    );
    // sRGB red has the textbook D50 Lab (LittleCMS: 54.29, 80.80, 69.89).
    t.transform(&[1.0f32, 0.0, 0.0, 0.0, 0.0, 1.0], &mut f)
        .unwrap();
    let red = Lab::new(f64::from(f[0]), f64::from(f[1]), f64::from(f[2]));
    assert!(
        delta_e_2000(red, Lab::new(54.29, 80.80, 69.89)) < 0.5,
        "{red:?}"
    );
    // Lab -> sRGB -> Lab round trip, through LittleCMS's own Lab profile too.
    let lab_lcms = Profile::new_from_slice(&data("LAB.icc")).unwrap();
    for lp in [&lab, &lab_lcms] {
        let back =
            Transform::new(lp, Layout::Lab, &srgb, Layout::Rgb, rel, Options::default()).unwrap();
        let mut rgb = [0f32; 3];
        back.transform(&[54.29f32, 80.80, 69.89], &mut rgb).unwrap();
        assert!(rgb[0] > 0.995 && rgb[1] < 0.01 && rgb[2] < 0.01, "{rgb:?}");
    }
}

#[test]
fn gray_round_trip_is_neutral() {
    let g = Profile::new_gray(rusty_icc::color::D65_XY, Curve::srgb());
    let srgb = Profile::new_srgb();
    let to = Transform::new(
        &srgb,
        Layout::Rgb,
        &g,
        Layout::Gray,
        Intent::Perceptual,
        Options::default(),
    )
    .unwrap();
    let from = Transform::new(
        &g,
        Layout::Gray,
        &srgb,
        Layout::Rgb,
        Intent::Perceptual,
        Options::default(),
    )
    .unwrap();
    for v in [0u8, 1, 17, 128, 200, 255] {
        let mut gr = [0u8];
        to.transform(&[v, v, v], &mut gr).unwrap();
        assert!(gr[0].abs_diff(v) <= 1, "{v} -> {}", gr[0]);
        let mut rgb = [0u8; 3];
        from.transform(&gr, &mut rgb).unwrap();
        assert!(rgb.iter().all(|c| c.abs_diff(v) <= 1), "{v} -> {rgb:?}");
    }
}

fn flare_camera() -> Profile {
    // The lut16 camera with a flare floor: its device black is L* ~ 6.
    let mut p = camera_lut16_lab();
    if let Some(Tag::Lut(Lut::Lut16(l))) = p.tag(*b"A2B0").cloned() {
        let mut l = l;
        for px in l.clut.data.chunks_exact_mut(3) {
            let lstar = f64::from(px[0]) / 65280.0 * 100.0;
            px[0] = ((6.0 + lstar * 0.94) / 100.0 * 65280.0).round() as u16;
        }
        p.set_tag(*b"A2B0", Tag::Lut(Lut::Lut16(l)));
    }
    p
}

#[test]
fn black_point_compensation() {
    let cam = flare_camera();
    let srgb_v2 = Profile::new_from_slice(&data("sRGB-v2-magic.icc")).unwrap();
    let black = [0u8, 0, 0];
    let run = |dst: &Profile, intent, bpc| {
        let t = Transform::new(
            &cam,
            Layout::Rgb,
            dst,
            Layout::Rgb,
            intent,
            Options {
                black_point_compensation: bpc,
            },
        )
        .unwrap();
        let mut o = [0u8; 3];
        t.transform(&black, &mut o).unwrap();
        o
    };
    let off = run(&srgb_v2, Intent::Perceptual, false);
    let on = run(&srgb_v2, Intent::Perceptual, true);
    println!("flare black -> v2 sRGB: without BPC {off:?}, with BPC {on:?}");
    assert!(off.iter().all(|&c| c > 10), "{off:?}");
    assert!(on.iter().all(|&c| c <= 1), "{on:?}");
    // Perceptual into a v4 destination compensates implicitly (LittleCMS).
    let v4 = run(&Profile::new_srgb(), Intent::Perceptual, false);
    assert!(v4.iter().all(|&c| c <= 1), "{v4:?}");
    // Absolute colorimetric never compensates.
    let abs = run(&srgb_v2, Intent::AbsoluteColorimetric, true);
    assert!(abs.iter().all(|&c| c > 10), "{abs:?}");
}

#[test]
fn proofing_and_gamut_check_against_srgb() {
    let p3 = Profile::new_display_p3();
    let srgb = Profile::new_srgb();
    let rel = Intent::RelativeColorimetric;
    let t = Transform::new_proofing(
        &p3,
        Layout::Rgb,
        &srgb,
        &p3,
        Layout::Rgb,
        rel,
        rel,
        Options::default(),
    )
    .unwrap();
    let src = [0.0f32, 1.0, 0.0, 0.5, 0.5, 0.5];
    let mut o = [0f32; 6];
    t.transform(&src, &mut o).unwrap();
    // P3 green clips to sRGB green, which is less saturated in P3.
    assert!(o[0] > 0.2 && o[1] < 1.0, "{o:?}");
    assert!(
        (o[3] - 0.5).abs() < 1e-3 && (o[4] - 0.5).abs() < 1e-3,
        "{o:?}"
    );
    let g = GamutCheck::new(&p3, Layout::Rgb, &srgb, rel, Options::default(), 1.0).unwrap();
    let mut flags = [false; 2];
    g.check(&src, &mut flags).unwrap();
    assert_eq!(flags, [true, false]);
}

#[test]
fn fogra39_soft_proof_bpc_and_paper_white() {
    let Some(bytes) = fogra39() else {
        println!("Coated FOGRA39 not available; skipped");
        return;
    };
    let fogra = Profile::new_from_slice(&bytes).unwrap();
    let srgb = Profile::new_srgb();
    let rel = Intent::RelativeColorimetric;
    // Relative proof keeps paper white white; absolute shows the paper.
    let white = [255u8, 255, 255];
    let proof = |pi| {
        let t = Transform::new_proofing(
            &srgb,
            Layout::Rgb,
            &fogra,
            &srgb,
            Layout::Rgb,
            rel,
            pi,
            Options::default(),
        )
        .unwrap();
        let mut o = [0u8; 3];
        t.transform(&white, &mut o).unwrap();
        o
    };
    let r = proof(rel);
    let a = proof(Intent::AbsoluteColorimetric);
    println!("FOGRA39 proof of sRGB white: relative {r:?}, absolute (paper) {a:?}");
    assert!(r.iter().all(|&c| c >= 253), "{r:?}");
    assert!(a.iter().all(|&c| c < 250), "paper white should show: {a:?}");
    // BPC: press black maps to display black.
    let k = [255u8, 255, 255, 255];
    let to_srgb = |bpc| {
        let t = Transform::new(
            &fogra,
            Layout::Cmyk,
            &Profile::new_from_slice(&data("sRGB-v2-magic.icc")).unwrap(),
            Layout::Rgb,
            rel,
            Options {
                black_point_compensation: bpc,
            },
        )
        .unwrap();
        let mut o = [0u8; 3];
        t.transform(&k, &mut o).unwrap();
        o
    };
    let (off, on) = (to_srgb(false), to_srgb(true));
    println!("FOGRA39 400% black -> sRGB v2: without BPC {off:?}, with BPC {on:?}");
    assert!(off[0] > 15 && on.iter().all(|&c| c <= 3), "{off:?} {on:?}");
    // Gamut: sRGB blue is outside FOGRA39, mid grey inside.
    let g = GamutCheck::new(&srgb, Layout::Rgb, &fogra, rel, Options::default(), 3.0).unwrap();
    let mut flags = [false; 2];
    g.check(&[0u8, 0, 255, 128, 128, 128], &mut flags).unwrap();
    assert_eq!(flags, [true, false]);
}

#[test]
fn curves_round_trip_all_parametric_types() {
    let curves = [
        Curve::gamma(2.2),
        Curve::parametric(1, &[2.4, 1.0, 0.0]).unwrap(),
        Curve::parametric(2, &[2.2, 0.9, 0.1, 0.02]).unwrap(),
        Curve::srgb(),
        Curve::parametric(4, &[2.4, 0.947, 0.052, 0.077, 0.04, 0.01, 0.002]).unwrap(),
        Curve::Table(vec![]),
        Curve::Table(vec![563]),
        Curve::Table((0..1024).map(|i| (i * 64) as u16).collect()),
    ];
    let mut p = Profile::new_srgb();
    for (i, c) in curves.iter().enumerate() {
        p.set_tag([b'c', b'v', b'0' + i as u8, b' '], Tag::Curve(c.clone()));
        for k in 1..=99 {
            let x = f64::from(k) / 100.0;
            let y = c.eval(x);
            if y > 0.02 && y < 0.98 {
                assert!((c.eval_inverse(y) - x).abs() < 2e-3, "{c:?} at {x}");
            }
        }
    }
    rt(&p);
    let _ = Xyz::default();
}
