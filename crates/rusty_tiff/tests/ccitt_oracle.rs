//! CCITT against the independent `fax` crate (the codec image-tiff uses):
//! its G4 encoder feeds our decoder, our G4/G3 encoders feed its decoders,
//! and G4 TIFFs decode identically in image-tiff.
#![allow(clippy::pedantic)] // test harness code: oracle plumbing, not library API

mod common;

use std::io::Cursor;

use common::{ours, theirs, Rng};
use fax::Color;
use rusty_tiff::ccitt::{self, Coding, Options};
use rusty_tiff::encoder::{colortype, Compression, TiffEncoder};

/// Random bilevel image (black = 1, packed MSB first) with runs from 0 to
/// beyond 2560 so every terminating, make-up and extended code appears.
fn image(w: u32, h: u32, seed: u64) -> Vec<u8> {
    let stride = w.div_ceil(8) as usize;
    let mut px = vec![0u8; stride * h as usize];
    let mut r = Rng(seed | 1);
    for y in 0..h as usize {
        if y > 0 && r.next() % 3 == 0 {
            // Near-copy of the row above: vertical-mode coverage.
            let (a, b) = px.split_at_mut(y * stride);
            b[..stride].copy_from_slice(&a[(y - 1) * stride..]);
            let x = (r.next() % u64::from(w)) as usize;
            b[x >> 3] ^= 0x80 >> (x & 7);
            continue;
        }
        let mut x = 0usize;
        let mut black = r.next() % 2 == 0;
        while x < w as usize {
            let run = match r.next() % 6 {
                0 => r.next() % 3000,
                1 => r.next() % 300,
                _ => r.next() % 16,
            } as usize;
            if black {
                for xx in x..(x + run).min(w as usize) {
                    px[y * stride + (xx >> 3)] |= 0x80 >> (xx & 7);
                }
            }
            x += run.max(1);
            black = !black;
        }
    }
    let pad = stride * 8 - w as usize;
    if pad > 0 {
        for y in 0..h as usize {
            px[y * stride + stride - 1] &= 0xFFu8 << pad;
        }
    }
    px
}

fn pels(px: &[u8], w: u32, y: usize) -> impl Iterator<Item = Color> + '_ {
    let stride = w.div_ceil(8) as usize;
    (0..w as usize).map(move |x| {
        if (px[y * stride + (x >> 3)] >> (7 - (x & 7))) & 1 == 1 {
            Color::Black
        } else {
            Color::White
        }
    })
}

fn render(lines: &[Vec<u16>], w: u32) -> Vec<u8> {
    let stride = w.div_ceil(8) as usize;
    let mut out = vec![0u8; stride * lines.len()];
    for (y, t) in lines.iter().enumerate() {
        for (x, c) in fax::decoder::pels(t, w as u16).enumerate() {
            if c == Color::Black {
                out[y * stride + (x >> 3)] |= 0x80 >> (x & 7);
            }
        }
    }
    out
}

const SIZES: [(u32, u32); 7] = [
    (1, 1),
    (7, 5),
    (64, 20),
    (1728, 30),
    (2000, 12),
    (2591, 6),
    (5300, 4),
];

#[test]
fn g4_fax_encoder_to_our_decoder() {
    let mut n = 0;
    for &(w, h) in &SIZES {
        for seed in 0..12 {
            let px = image(w, h, seed * 7919 + u64::from(w));
            let mut enc = fax::encoder::Encoder::new(fax::VecWriter::new());
            for y in 0..h as usize {
                enc.encode_line(pels(&px, w, y), w as u16).unwrap();
            }
            let data = enc.finish().unwrap().finish();
            let opts = Options::new(Coding::Group4, w);
            let ours = ccitt::decode_g4(&data, w, h, &opts).unwrap();
            assert_eq!(ours, px, "{w}x{h} seed {seed}");
            // And image-tiff, through a TIFF wrapper (WhiteIsZero, compression 4).
            let tif = fax::tiff::wrap(&data, w, h);
            let a = common::ours(&tif).unwrap();
            let b = theirs(&tif).unwrap();
            assert_eq!(a, b, "{w}x{h} TIFF");
            n += 1;
        }
    }
    println!("G4: {n} fax-crate-encoded images decoded identically (raw + TIFF vs image-tiff)");
}

#[test]
fn g4_our_encoder_to_fax_decoder() {
    let mut n = 0;
    for &(w, h) in &SIZES {
        for seed in 0..12 {
            let px = image(w, h, seed * 104_729 + u64::from(h));
            let data = ccitt::encode(&px, &Options::new(Coding::Group4, w).with_rows(h)).unwrap();
            let mut lines = Vec::new();
            fax::decoder::decode_g4(data.iter().copied(), w as u16, Some(h as u16), |t| {
                lines.push(t.to_vec());
            })
            .unwrap();
            assert_eq!(render(&lines, w), px, "{w}x{h}");
            n += 1;
        }
    }
    println!("G4: {n} images from our encoder decoded by the fax crate");
}

#[test]
fn g3_1d_our_encoder_to_fax_decoder() {
    let mut n = 0;
    for &(w, h) in &SIZES {
        for seed in 0..12 {
            let px = image(w, h, seed * 31 + 5);
            let data =
                ccitt::encode(&px, &Options::new(Coding::Group3_1D, w).with_rows(h)).unwrap();
            let mut lines = Vec::new();
            fax::decoder::decode_g3(data.iter().copied(), |t| lines.push(t.to_vec()));
            lines.truncate(h as usize);
            assert_eq!(render(&lines, w), px, "{w}x{h}");
            // Our decoder reads it back too (TIFF compression 3 path).
            let back =
                ccitt::decode_g3(&data, w, h, false, &Options::new(Coding::Group3_1D, w)).unwrap();
            assert_eq!(back, px);
            n += 1;
        }
    }
    println!("G3 1-D: {n} images from our encoder decoded by the fax crate and by us");
}

#[test]
fn tiff_fax_pages_match_image_tiff() {
    // Multipage fax TIFF: G4 pages (image-tiff decodes them) and one G3 page
    // (image-tiff does not decode compression 3: we check it against source).
    let (w, h) = (1728u32, 40u32);
    let pages: Vec<Vec<u8>> = (0..4).map(|s| image(w, h, 1000 + s)).collect();
    let mut f = Cursor::new(Vec::new());
    {
        let mut e = TiffEncoder::new(&mut f).unwrap();
        for p in &pages {
            e.new_image::<colortype::Gray1>(w, h)
                .unwrap()
                .with_compression(Compression::Fax4)
                .write_data(p)
                .unwrap();
        }
    }
    let file = f.into_inner();
    let a = ours(&file).unwrap();
    let b = theirs(&file).unwrap();
    assert_eq!(a.len(), 4);
    assert_eq!(a, b);
    for (pg, src) in a.iter().zip(&pages) {
        assert_eq!(&pg.bytes, src);
    }
    let mut f = Cursor::new(Vec::new());
    TiffEncoder::new(&mut f)
        .unwrap()
        .with_compression(Compression::Fax3)
        .write_image::<colortype::Gray1>(w, h, &pages[0])
        .unwrap();
    let a = ours(&f.into_inner()).unwrap();
    assert_eq!(a[0].bytes, pages[0]);
    println!("fax TIFF: 4-page G4 identical to image-tiff; G3 page equals source");
}
