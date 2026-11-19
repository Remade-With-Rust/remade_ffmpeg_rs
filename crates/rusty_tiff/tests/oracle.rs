//! Oracle tests: every file decodes byte-identically with rusty_tiff and with
//! image-tiff 0.11 (`tiff`), and both equal the source pixels.
//!
//! Run with `--nocapture` to see the count table.
#![allow(clippy::pedantic)] // test harness code: oracle plumbing, not library API

mod common;

use std::io::Cursor;

use common::{ours, theirs, to_big_endian, Rng};
use rusty_tiff::encoder::{colortype, ColorType, Compression, Predictor, TiffEncoder, TiffSample};
use rusty_tiff::tags::photometric;

trait Gen: TiffSample {
    fn gen(r: &mut Rng, i: usize) -> Self;
    fn invert_bytes(b: &mut [u8]);
    fn from_byte(b: u8) -> Self;
}

macro_rules! gen_int {
    ($($t:ty),*) => {$(
        impl Gen for $t {
            fn gen(r: &mut Rng, i: usize) -> Self {
                // Half smooth ramps (compressible), half noise.
                if (i / 5) % 2 == 0 {
                    (i as u64).wrapping_mul(2654435761) as $t
                } else {
                    r.next() as $t
                }
            }
            fn invert_bytes(b: &mut [u8]) {
                b.iter_mut().for_each(|x| *x = !*x);
            }
            fn from_byte(b: u8) -> Self {
                b as $t
            }
        }
    )*};
}
gen_int!(u8, i8, u16, i16, u32, i32, u64, i64);

const F32_SPECIAL: [f32; 6] = [0.0, -0.0, f32::INFINITY, f32::NAN, 1e-40, -3.5];
impl Gen for f32 {
    fn gen(r: &mut Rng, i: usize) -> Self {
        if i % 97 < 6 {
            F32_SPECIAL[i % 97]
        } else if (i / 5) % 2 == 0 {
            i as f32 * 0.37 - 10.0
        } else {
            f32::from_bits((r.next() >> 32) as u32 & 0x3FFF_FFFF)
        }
    }
    fn invert_bytes(b: &mut [u8]) {
        let v = f32::from_ne_bytes(b.try_into().unwrap());
        b.copy_from_slice(&(1.0 - v).to_ne_bytes());
    }
    fn from_byte(b: u8) -> Self {
        Self::from(b)
    }
}
impl Gen for f64 {
    fn gen(r: &mut Rng, i: usize) -> Self {
        if i % 89 == 0 {
            f64::NAN
        } else if (i / 5) % 2 == 0 {
            i as f64 * 0.37 - 10.0
        } else {
            f64::from_bits(r.next() & 0x3FFF_FFFF_FFFF_FFFF)
        }
    }
    fn invert_bytes(b: &mut [u8]) {
        let v = f64::from_ne_bytes(b.try_into().unwrap());
        b.copy_from_slice(&(1.0 - v).to_ne_bytes());
    }
    fn from_byte(b: u8) -> Self {
        Self::from(b)
    }
}

#[derive(Clone, Copy, Debug)]
struct Params {
    w: u32,
    h: u32,
    comp: Compression,
    pred: Predictor,
    layout: Layout,
    planar: bool,
    min_is_white: bool,
    big: bool,
}

#[derive(Clone, Copy, Debug)]
enum Layout {
    DefaultStrips,
    Strips3,
    Tiles16,
    Tiles32x16,
}

struct Made {
    file: Vec<u8>,
    /// Expected `read_image` output (native-endian bytes).
    expected: Vec<u8>,
}

#[inline(never)]
fn make<C: ColorType>(p: &Params) -> Option<Made>
where
    C::Inner: Gen,
{
    let bits = C::BITS_PER_SAMPLE as usize;
    let spp = C::SAMPLES as usize;
    let is_float = C::SAMPLE_FORMAT == 3;
    match p.pred {
        Predictor::Horizontal if bits < 8 || is_float => return None,
        Predictor::FloatingPoint if !is_float => return None,
        _ => {}
    }
    if p.planar && spp == 1 {
        return None;
    }
    if p.min_is_white && (C::PHOTOMETRIC != photometric::BLACK_IS_ZERO || bits == 16 && is_float) {
        return None;
    }
    let (w, h) = (p.w as usize, p.h as usize);
    let mut r = Rng(0x9E37_79B9_7F4A_7C15 ^ (w * 31 + h) as u64);
    let row = (w * spp * bits).div_ceil(8);
    let n = if bits < 8 { row * h } else { w * h * spp };
    let mut data: Vec<C::Inner> = (0..n).map(|i| C::Inner::gen(&mut r, i)).collect();
    let mut src = Vec::with_capacity(n * C::Inner::BYTES);
    if bits < 8 {
        // Clear row padding bits: they are not pixels.
        let pad = row * 8 - w * spp * bits;
        let mut bytes: Vec<u8> = Vec::new();
        for d in &data {
            d.put_ne(&mut bytes);
        }
        for y in 0..h {
            if pad > 0 {
                bytes[y * row + row - 1] &= 0xFFu8 << pad;
            }
        }
        // Sub-byte types take u8 input: rebuild the samples from the bytes.
        data = bytes.iter().map(|&b| C::Inner::from_byte(b)).collect();
        src = bytes;
    } else {
        for d in &data {
            d.put_ne(&mut src);
        }
    }
    let mut file = Cursor::new(Vec::new());
    {
        let mut enc = if p.big {
            TiffEncoder::new_big(&mut file).unwrap()
        } else {
            TiffEncoder::new(&mut file).unwrap()
        };
        let mut img = enc
            .new_image::<C>(p.w, p.h)
            .unwrap()
            .with_compression(p.comp)
            .with_predictor(p.pred);
        match p.layout {
            Layout::DefaultStrips => {}
            Layout::Strips3 => img.rows_per_strip(3),
            Layout::Tiles16 => img.tiles(16, 16),
            Layout::Tiles32x16 => img.tiles(32, 16),
        }
        img.planar(p.planar);
        if p.min_is_white {
            img.photometric(photometric::WHITE_IS_ZERO);
        }
        if C::PHOTOMETRIC == photometric::PALETTE {
            img.colormap(palette(bits));
        }
        img.write_data(&data).unwrap();
    }
    let mut expected = src.clone();
    if p.min_is_white {
        if bits < 8 {
            expected.iter_mut().for_each(|x| *x = !*x);
        } else {
            let sb = bits / 8;
            for px in expected.chunks_exact_mut(sb * spp) {
                C::Inner::invert_bytes(&mut px[..sb]);
            }
        }
    }
    if C::PHOTOMETRIC == photometric::PALETTE {
        let map = palette(bits);
        let entries = 1usize << bits;
        let mut out = Vec::new();
        for y in 0..h {
            for x in 0..w {
                let bit = x * bits;
                let byte = src[y * row + bit / 8];
                let idx = if bits == 8 {
                    byte as usize
                } else {
                    ((byte >> (8 - bits - bit % 8)) & ((1 << bits) - 1) as u8) as usize
                };
                for c in 0..3 {
                    out.extend_from_slice(&map[c * entries + idx].to_ne_bytes());
                }
            }
        }
        expected = out;
    }
    Some(Made {
        file: file.into_inner(),
        expected,
    })
}

fn palette(bits: usize) -> Vec<u16> {
    let n = 1usize << bits;
    (0..3 * n).map(|i| (i as u16).wrapping_mul(40503)).collect()
}

#[derive(Default)]
struct Tally {
    files: usize,
    identical: usize,
    oracle_unsupported: usize,
    oracle_panics: Vec<String>,
    oracle_errors: std::collections::BTreeMap<String, usize>,
    big_endian: usize,
    bigtiff: usize,
}

#[inline(never)]
fn check(name: &str, p: &Params, m: &Made, t: &mut Tally) {
    t.files += 1;
    let o = ours(&m.file).unwrap_or_else(|e| panic!("{name} {p:?}: rusty_tiff failed: {e}"));
    assert_eq!(o.len(), 1);
    assert!(
        o[0].bytes == m.expected,
        "{name} {p:?}: rusty_tiff output differs from source"
    );
    match theirs(&m.file) {
        Ok(th) => {
            assert!(
                th[0].bytes == o[0].bytes,
                "{name} {p:?}: differs from image-tiff ({} vs {} bytes)",
                th[0].bytes.len(),
                o[0].bytes.len()
            );
            t.identical += 1;
        }
        Err(e) => {
            if e.starts_with("PANIC") {
                t.oracle_panics.push(format!("{name} {p:?}"));
            }
            let key: String = e.chars().take(70).collect();
            *t.oracle_errors.entry(key).or_default() += 1;
            t.oracle_unsupported += 1;
        }
    }
    if p.big {
        t.bigtiff += 1;
    }
    // Big-endian twin of the same file, where the conversion is a byte swap.
    if !p.big {
        if let Some(be) = to_big_endian(&m.file) {
            t.files += 1;
            t.big_endian += 1;
            let ob = ours(&be).unwrap_or_else(|e| panic!("{name} {p:?} BE: {e}"));
            assert!(
                ob[0].bytes == m.expected,
                "{name} {p:?} BE: differs from source"
            );
            match theirs(&be) {
                Ok(th) => {
                    assert!(
                        th[0].bytes == ob[0].bytes,
                        "{name} {p:?} BE: differs from image-tiff"
                    );
                    t.identical += 1;
                }
                Err(e) => {
                    if e.starts_with("PANIC") {
                        t.oracle_panics.push(format!("{name} {p:?} BE"));
                    }
                    let key: String = e.chars().take(70).collect();
                    *t.oracle_errors.entry(key).or_default() += 1;
                    t.oracle_unsupported += 1;
                }
            }
        }
    }
}

type MakeFn = fn(&Params) -> Option<Made>;

macro_rules! cases {
    ($($ct:ident),*) => {
        vec![$((stringify!($ct), make::<colortype::$ct> as MakeFn)),*]
    };
}

#[test]
fn house_encoded_matrix_matches_image_tiff() {
    let mut t = Tally::default();
    let cases = cases!(
        Gray1,
        Gray2,
        Gray4,
        Gray8,
        Gray16,
        Gray32,
        Gray64,
        GrayI8,
        GrayI16,
        GrayI32,
        GrayI64,
        Gray16Float,
        Gray32Float,
        Gray64Float,
        GrayA8,
        GrayA16,
        RGB8,
        RGB16,
        RGB32,
        RGB16Float,
        RGB32Float,
        RGB64Float,
        RGBA8,
        RGBA16,
        RGBA32Float,
        CMYK8,
        CMYK16,
        CMYKA8,
        CMYKA16,
        Palette4,
        Palette8
    );
    let sizes = [(37u32, 29u32), (1, 1), (130, 5)];
    let comps = [
        Compression::None,
        Compression::Lzw,
        Compression::deflate(),
        Compression::PackBits,
    ];
    let preds = [
        Predictor::None,
        Predictor::Horizontal,
        Predictor::FloatingPoint,
    ];
    let layouts = [
        Layout::DefaultStrips,
        Layout::Strips3,
        Layout::Tiles16,
        Layout::Tiles32x16,
    ];
    for &(w, h) in &sizes {
        for &comp in &comps {
            for &pred in &preds {
                for &layout in &layouts {
                    for planar in [false, true] {
                        for min_is_white in [false, true] {
                            for big in [false, true] {
                                let p = Params {
                                    w,
                                    h,
                                    comp,
                                    pred,
                                    layout,
                                    planar,
                                    min_is_white,
                                    big,
                                };
                                for (name, f) in &cases {
                                    if let Some(m) = f(&p) {
                                        check(name, &p, &m, &mut t);
                                    }
                                }
                            }
                        }
                    }
                }
            }
        }
    }
    println!(
        "house-encoded matrix: {} files ({} big-endian twins, {} BigTIFF); \
         {} byte-identical to image-tiff, {} not decodable by image-tiff \
         (checked against source only); all {} equal to source",
        t.files, t.big_endian, t.bigtiff, t.identical, t.oracle_unsupported, t.files
    );
    for (e, n) in &t.oracle_errors {
        println!("  image-tiff error x{n}: {e}");
    }
    println!(
        "image-tiff PANICKED on {} of them, e.g.:",
        t.oracle_panics.len()
    );
    for p in t.oracle_panics.iter().take(6) {
        println!("  {p}");
    }
    assert_eq!(t.identical + t.oracle_unsupported, t.files);
}

/// Encode with image-tiff, decode with both, compare with each other and
/// the source, for every compression x strip size x predictor.
fn tiff_encoded<C>(spp: usize, preds: &[tiff::tags::Predictor]) -> usize
where
    C: tiff::encoder::colortype::ColorType,
    C::Inner: Gen,
    [C::Inner]: tiff::encoder::TiffValue,
{
    use tiff::encoder::{Compression as TC, DeflateLevel, TiffEncoder as TE};
    let (w, h) = (53u32, 31u32);
    let mut r = Rng(42);
    let n = (w * h) as usize * spp;
    let data: Vec<C::Inner> = (0..n).map(|i| C::Inner::gen(&mut r, i)).collect();
    let mut src = Vec::new();
    for d in &data {
        d.put_ne(&mut src);
    }
    let mut files = 0;
    for comp in [
        TC::Uncompressed,
        TC::Lzw,
        TC::Deflate(DeflateLevel::default()),
        TC::Packbits,
    ] {
        for rps in [None, Some(1u32), Some(7)] {
            for &pred in preds {
                let mut f = Cursor::new(Vec::new());
                {
                    let mut e = TE::new(&mut f)
                        .unwrap()
                        .with_predictor(pred)
                        .with_compression(comp);
                    let mut img = e.new_image::<C>(w, h).unwrap();
                    if let Some(rps) = rps {
                        img.rows_per_strip(rps).unwrap();
                    }
                    img.write_data(&data).unwrap();
                }
                let file = f.into_inner();
                let o = ours(&file).unwrap();
                let t = theirs(&file).unwrap();
                assert!(
                    o[0].bytes == t[0].bytes,
                    "{} differs",
                    std::any::type_name::<C>()
                );
                assert!(
                    o[0].bytes == src,
                    "{} != source",
                    std::any::type_name::<C>()
                );
                files += 1;
            }
        }
    }
    files
}

/// Files written by image-tiff's own encoder, decoded by both.
#[test]
fn image_tiff_encoded_files_match() {
    use tiff::encoder::colortype as tc;
    use tiff::tags::Predictor as TP;
    let both = [TP::None, TP::Horizontal];
    let none = [TP::None];
    let files = tiff_encoded::<tc::Gray8>(1, &both)
        + tiff_encoded::<tc::Gray16>(1, &both)
        + tiff_encoded::<tc::Gray32>(1, &both)
        + tiff_encoded::<tc::Gray64>(1, &both)
        + tiff_encoded::<tc::GrayI8>(1, &both)
        + tiff_encoded::<tc::GrayI16>(1, &both)
        + tiff_encoded::<tc::GrayI32>(1, &both)
        + tiff_encoded::<tc::RGB8>(3, &both)
        + tiff_encoded::<tc::RGB16>(3, &both)
        + tiff_encoded::<tc::RGBA8>(4, &both)
        + tiff_encoded::<tc::RGBA16>(4, &both)
        + tiff_encoded::<tc::CMYK8>(4, &both)
        + tiff_encoded::<tc::CMYK16>(4, &both)
        + tiff_encoded::<tc::Gray32Float>(1, &none)
        + tiff_encoded::<tc::Gray64Float>(1, &none)
        + tiff_encoded::<tc::RGB32Float>(3, &none);
    println!("image-tiff-encoded: {files} files byte-identical to image-tiff and source");
}

/// A multipage file mixing types and compressions: both decoders walk the
/// same pages and agree on every one.
#[test]
fn multipage_matches() {
    let mut f = Cursor::new(Vec::new());
    let g8: Vec<u8> = (0..40 * 30).map(|i| (i * 7) as u8).collect();
    let rgb16: Vec<u16> = (0..20 * 10 * 3).map(|i| (i * 977) as u16).collect();
    let fl: Vec<f32> = (0..17 * 9).map(|i| i as f32 / 3.0).collect();
    {
        let mut e = TiffEncoder::new(&mut f).unwrap();
        e.write_image::<colortype::Gray8>(40, 30, &g8).unwrap();
        let mut i = e
            .new_image::<colortype::RGB16>(20, 10)
            .unwrap()
            .with_compression(Compression::Lzw)
            .with_predictor(Predictor::Horizontal);
        i.tiles(16, 16);
        i.write_data(&rgb16).unwrap();
        e.new_image::<colortype::Gray32Float>(17, 9)
            .unwrap()
            .with_compression(Compression::deflate())
            .with_predictor(Predictor::FloatingPoint)
            .write_data(&fl)
            .unwrap();
    }
    let file = f.into_inner();
    let o = ours(&file).unwrap();
    let t = theirs(&file).unwrap();
    assert_eq!(o.len(), 3);
    assert_eq!(o, t);
    assert_eq!(o[0].bytes, g8);
    println!("multipage: 3 pages identical");
}
