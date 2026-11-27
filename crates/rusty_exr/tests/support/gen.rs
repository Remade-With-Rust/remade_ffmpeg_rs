//! Deterministic test images shared by the byte-identity test and the
//! out-of-tree harness that derived its expected values from the `exr`
//! crate. Integer arithmetic only, so every platform builds the same image.

#![allow(dead_code)]

use rusty_exr::{ChannelDesc, Compression, Image, LineOrder, SampleType, Samples};

struct Rng(u64);

impl Rng {
    fn next(&mut self) -> u64 {
        self.0 ^= self.0 << 13;
        self.0 ^= self.0 >> 7;
        self.0 ^= self.0 << 17;
        self.0
    }
}

/// A smooth field with noise and the odd bright pixel, as a float.
fn field(x: u32, y: u32, k: u32, r: &mut Rng) -> f32 {
    let a = (x * (3 + k) + y * (5 + 2 * k)) % 211;
    let b = (x * y + k * 17) % 64;
    let v = a as f32 / 97.0 + b as f32 / 256.0 + (r.next() % 64) as f32 / 4096.0;
    if (x + 3 * y + k) % 97 == 0 {
        v * 40.0
    } else {
        v
    }
}

fn channel(img: &mut Image, name: &str, ty: SampleType, k: u32, r: &mut Rng) {
    let (w, h) = (img.width() as u32, img.height() as u32);
    let mut vals = Vec::with_capacity((w * h) as usize);
    for y in 0..h {
        for x in 0..w {
            vals.push(field(x, y, k, r));
        }
    }
    let samples = match ty {
        SampleType::F16 => Samples::F16(vals.iter().map(|&v| rusty_exr::f32_to_f16(v)).collect()),
        SampleType::F32 => Samples::F32(vals),
        SampleType::U32 => Samples::U32(vals.iter().map(|&v| (v * 1000.0) as u32).collect()),
    };
    img.channels.push(rusty_exr::Channel {
        desc: ChannelDesc::new(name, ty),
        width: w as usize,
        height: h as usize,
        samples,
    });
}

pub fn image(kind: &str, w: u32, h: u32) -> Image {
    use SampleType::{F16, F32, U32};
    let mut r = Rng(0x9e37_79b9_7f4a_7c15 ^ u64::from(w * 131 + h));
    let mut img = Image::new(w, h);
    match kind {
        "rgba16" => {
            for (k, n) in ["A", "B", "G", "R"].iter().enumerate() {
                channel(&mut img, n, F16, k as u32, &mut r);
            }
        }
        "layers" => {
            channel(&mut img, "A", F16, 0, &mut r);
            channel(&mut img, "B", F32, 1, &mut r);
            channel(&mut img, "G", F32, 2, &mut r);
            channel(&mut img, "R", F32, 3, &mut r);
            channel(&mut img, "diffuse.B", F16, 4, &mut r);
            channel(&mut img, "diffuse.G", F16, 5, &mut r);
            channel(&mut img, "diffuse.R", F16, 6, &mut r);
            channel(&mut img, "id", U32, 7, &mut r);
            channel(&mut img, "mask.Y", F32, 8, &mut r);
        }
        "linear" => {
            channel(&mut img, "B", F16, 0, &mut r);
            channel(&mut img, "G", F16, 1, &mut r);
            channel(&mut img, "R", F16, 2, &mut r);
            channel(&mut img, "depth", F16, 3, &mut r);
            channel(&mut img, "hdr.R", F16, 4, &mut r);
            for c in &mut img.channels {
                if c.desc.name == "B" {
                    c.desc.p_linear = true;
                }
                if c.desc.name == "hdr.R" {
                    if let Samples::F16(v) = &mut c.samples {
                        for (i, x) in v.iter_mut().enumerate() {
                            *x = match i % 13 {
                                0 => 0x7c00,
                                1 => 0xfc00,
                                2 => 0x7e00,
                                3 => 0x0001,
                                4 => 0x7bff,
                                _ => *x,
                            };
                        }
                    }
                }
            }
        }
        k => panic!("kind {k}"),
    }
    img
}

pub struct Case {
    pub kind: &'static str,
    pub w: u32,
    pub h: u32,
    pub compression: Compression,
    pub tiles: Option<(u32, u32)>,
    pub line_order: LineOrder,
    pub dwa_level: Option<f32>,
}

pub fn cases() -> Vec<Case> {
    use Compression as C;
    let mut v = Vec::new();
    let mut add = |kind, w, h, compression, tiles, line_order, dwa_level| {
        v.push(Case {
            kind,
            w,
            h,
            compression,
            tiles,
            line_order,
            dwa_level,
        });
    };
    let inc = LineOrder::Increasing;
    let any = LineOrder::Random;
    for c in [
        C::None,
        C::Rle,
        C::Zips,
        C::Zip,
        C::Piz,
        C::Pxr24,
        C::B44,
        C::B44a,
        C::Dwaa,
        C::Dwab,
    ] {
        add("rgba16", 61, 37, c, None, inc, None);
        add("layers", 45, 70, c, None, inc, None);
    }
    for c in [C::B44, C::B44a, C::Dwaa, C::Dwab] {
        add("linear", 23, 41, c, None, inc, None);
    }
    add("rgba16", 61, 37, C::Dwaa, None, inc, Some(10.0));
    add("layers", 45, 70, C::Dwab, None, inc, Some(150.0));
    for c in [C::Rle, C::Zip, C::Piz, C::B44a, C::Dwab] {
        add("rgba16", 130, 70, c, Some((64, 64)), any, None);
    }
    add("layers", 45, 70, C::Rle, Some((64, 64)), any, None);
    add("rgba16", 61, 37, C::Rle, Some((16, 8)), inc, None);
    v
}

pub fn options(c: &Case) -> rusty_exr::EncodeOptions {
    rusty_exr::EncodeOptions {
        compression: c.compression,
        tiles: c.tiles,
        line_order: c.line_order,
        dwa_level: c.dwa_level,
        ..rusty_exr::EncodeOptions::default()
    }
}

pub fn fnv(b: &[u8]) -> u64 {
    let mut h = 0xcbf2_9ce4_8422_2325u64;
    for &x in b {
        h ^= u64::from(x);
        h = h.wrapping_mul(0x0000_0100_0000_01b3);
    }
    h
}
