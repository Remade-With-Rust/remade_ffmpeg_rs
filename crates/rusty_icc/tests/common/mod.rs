//! Shared test helpers: test-data loading, the optional FOGRA39 lookup and
//! synthetic camera-style LUT profiles.
#![allow(dead_code)]

use rusty_icc::color::{lab_to_xyz, xyz_to_lab, Lab, Xyz, D50};
use rusty_icc::{AbLut, Clut, ColorSpace, Curve, LegacyLut, Lut, Profile, ProfileClass, Tag};
use std::path::PathBuf;

pub fn data(name: &str) -> Vec<u8> {
    let p = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("tests")
        .join("data")
        .join(name);
    std::fs::read(&p).unwrap_or_else(|e| panic!("{}: {e}", p.display()))
}

pub const DATA_PROFILES: &[&str] = &[
    "CGATS001Compat-v2-micro.icc",
    "LAB.icc",
    "ProPhoto-v2-micro.icc",
    "sGrey-v2-magic.icc",
    "sGrey-v4.icc",
    "sRGB-v2-magic.icc",
    "sRGB-v4.icc",
    "sRGB_v4_ICC_preference.icc",
];

/// Coated FOGRA39 (Adobe). Its licence forbids modification and limits
/// redistribution, so it is NOT committed: set `RUSTY_ICC_FOGRA39` to a copy,
/// or it is picked up from the cargo registry if a crate that bundles it
/// (`printpdf`) has been downloaded. Tests that need it skip without it.
pub fn fogra39() -> Option<Vec<u8>> {
    if let Ok(p) = std::env::var("RUSTY_ICC_FOGRA39") {
        return std::fs::read(p).ok();
    }
    let home = std::env::var_os("CARGO_HOME")
        .map(PathBuf::from)
        .or_else(|| {
            std::env::var_os("USERPROFILE")
                .or_else(|| std::env::var_os("HOME"))
                .map(|h| PathBuf::from(h).join(".cargo"))
        })?;
    let src = home.join("registry").join("src");
    for idx in std::fs::read_dir(src).ok()?.flatten() {
        let Ok(crates) = std::fs::read_dir(idx.path()) else {
            continue;
        };
        let mut found: Vec<PathBuf> = crates
            .flatten()
            .filter(|e| e.file_name().to_string_lossy().starts_with("printpdf-"))
            .map(|e| e.path().join("src").join("res").join("CoatedFOGRA39.icc"))
            .filter(|p| p.exists())
            .collect();
        found.sort();
        if let Some(p) = found.pop() {
            return std::fs::read(p).ok();
        }
    }
    None
}

/// A made-up camera: gamma-2.2 encoded RGB with channel cross-talk.
const CAMERA_TO_XYZ_D50: [[f64; 3]; 3] =
    [[0.52, 0.31, 0.11], [0.24, 0.71, 0.05], [0.03, 0.12, 0.66]];

fn camera_xyz(rgb: [f64; 3]) -> Xyz {
    let lin = rgb.map(|v| v.powf(2.2));
    // A mild non-linearity (shoulder) a matrix cannot express, so the CLUT
    // carries real content.
    let lin = lin.map(|v| v * (1.15 - 0.15 * v));
    let m = CAMERA_TO_XYZ_D50;
    Xyz::new(
        m[0][0] * lin[0] + m[0][1] * lin[1] + m[0][2] * lin[2],
        m[1][0] * lin[0] + m[1][1] * lin[1] + m[1][2] * lin[2],
        m[2][0] * lin[0] + m[2][1] * lin[1] + m[2][2] * lin[2],
    )
}

fn input_template(version: u32, pcs: ColorSpace) -> Profile {
    let mut p = Profile::new_srgb();
    for s in [
        *b"rXYZ", *b"gXYZ", *b"bXYZ", *b"rTRC", *b"gTRC", *b"bTRC", *b"chad", *b"desc", *b"cprt",
    ] {
        p.remove_tag(s);
    }
    p.header.version = version;
    p.header.class = ProfileClass::Input;
    p.header.pcs = pcs;
    p
}

/// A v2 input-class RGB profile whose `A2B0` is a `lut16` (17-point CLUT,
/// Lab PCS in the legacy 16-bit encoding), sampled from a synthetic camera.
pub fn camera_lut16_lab() -> Profile {
    let mut p = input_template(0x0210_0000, ColorSpace::Lab);
    p.set_description("synthetic camera lut16 Lab (rusty_icc tests)");
    p.set_copyright("No copyright, use freely");
    let g = 17u8;
    let mut data = Vec::new();
    for i in 0..g {
        for j in 0..g {
            for k in 0..g {
                let rgb = [i, j, k].map(|v| f64::from(v) / f64::from(g - 1));
                let lab = xyz_to_lab(camera_xyz(rgb), D50);
                // Legacy 16-bit Lab: L 0..100 -> 0..0xFF00, a/b +128 * 256.
                let enc = [
                    lab.l / 100.0 * 65280.0,
                    (lab.a + 128.0) * 256.0,
                    (lab.b + 128.0) * 256.0,
                ];
                data.extend(enc.map(|v| v.round().clamp(0.0, 65535.0) as u16));
            }
        }
    }
    let ramp: Vec<u16> = (0..256).map(|i| (i * 257) as u16).collect();
    let lut = LegacyLut {
        matrix: [1.0, 0.0, 0.0, 0.0, 1.0, 0.0, 0.0, 0.0, 1.0],
        input_tables: vec![ramp.clone(), ramp.clone(), ramp],
        clut: Clut {
            grid: vec![g; 3],
            outputs: 3,
            data,
            precision: 2,
        },
        output_tables: vec![vec![0, 65535]; 3],
    };
    p.set_tag(*b"A2B0", Tag::Lut(Lut::Lut16(lut)));
    p
}

/// A v4 input-class RGB profile whose `A2B0` is an `mAB ` with A curves
/// (gamma 2.2), a 9-point 8-bit CLUT carrying the shoulder, M curves, a
/// matrix with offsets and B curves, XYZ PCS.
pub fn camera_mab_xyz() -> Profile {
    let mut p = input_template(0x0440_0000, ColorSpace::Xyz);
    p.set_description("synthetic camera mAB XYZ (rusty_icc tests)");
    p.set_copyright("No copyright, use freely");
    let g = 9u8;
    let mut data = Vec::new();
    for i in 0..g {
        for j in 0..g {
            for k in 0..g {
                let lin = [i, j, k].map(|v| f64::from(v) / f64::from(g - 1));
                let s = lin.map(|v| v * (1.15 - 0.15 * v));
                data.extend(s.map(|v| ((v * 255.0).round() as u16) * 257));
            }
        }
    }
    // Normalised XYZ (u1Fixed15: 1.0 at 32768/65535) = M * shaped + off.
    let k = 32768.0 / 65535.0;
    let m = CAMERA_TO_XYZ_D50;
    let mut matrix = [0.0; 12];
    for r in 0..3 {
        for c in 0..3 {
            matrix[r * 3 + c] = (m[r][c] * k * 65536.0).round() / 65536.0;
        }
    }
    matrix[9] = (0.001 * 65536.0f64).round() / 65536.0;
    let lut = AbLut {
        inputs: 3,
        outputs: 3,
        a: Some(vec![
            Curve::gamma(2.2),
            Curve::gamma(2.2),
            Curve::gamma(2.2),
        ]),
        clut: Some(Clut {
            grid: vec![g; 3],
            outputs: 3,
            data,
            precision: 1,
        }),
        m: Some(vec![
            Curve::identity(),
            Curve::identity(),
            Curve::identity(),
        ]),
        matrix: Some(matrix),
        b: vec![Curve::gamma(1.0), Curve::gamma(1.0), Curve::gamma(1.0)],
    };
    p.set_tag(*b"A2B0", Tag::Lut(Lut::AToB(lut)));
    p
}

/// sRGB written as an `mAB ` (M curves = sRGB curve, matrix = colorants,
/// XYZ PCS): through the LUT path it must equal the matrix-shaper path.
pub fn srgb_as_mab() -> Profile {
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

pub fn lab_d50(xyz: Xyz) -> Lab {
    xyz_to_lab(xyz, D50)
}

pub fn xyz_d50(lab: Lab) -> Xyz {
    lab_to_xyz(lab, D50)
}

/// Deterministic xorshift64*.
pub struct Rng(pub u64);

impl Rng {
    pub fn next(&mut self) -> u64 {
        self.0 ^= self.0 >> 12;
        self.0 ^= self.0 << 25;
        self.0 ^= self.0 >> 27;
        self.0.wrapping_mul(0x2545_F491_4F6C_DD1D)
    }
    pub fn below(&mut self, n: usize) -> usize {
        (self.next() % n.max(1) as u64) as usize
    }
}
