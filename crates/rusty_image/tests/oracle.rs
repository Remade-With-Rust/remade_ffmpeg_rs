//! The oracle gate: every decoder against the `image` crate 0.25, every probe
//! against `imagesize`, over deterministic corpora of generated files that
//! cover each format's layouts. A file passes when both decoders agree:
//! both refuse it, or both decode it to the same layout and the same bytes.
//!
//! Each test prints its tally (`cargo test -- --nocapture`).

use std::io::Cursor;

use image::{ColorType, DynamicImage, ImageFormat};
use rusty_image::{Format, Image, PixelFormat};

// ── deterministic randomness ─────────────────────────────────────────────────

struct Rng(u64);
impl Rng {
    fn next(&mut self) -> u64 {
        self.0 ^= self.0 << 13;
        self.0 ^= self.0 >> 7;
        self.0 ^= self.0 << 17;
        self.0
    }
    fn below(&mut self, n: u64) -> u64 {
        self.next() % n.max(1)
    }
    fn byte(&mut self) -> u8 {
        self.next() as u8
    }
    fn bytes(&mut self, n: usize) -> Vec<u8> {
        (0..n).map(|_| self.byte()).collect()
    }
    fn pick<T: Copy>(&mut self, v: &[T]) -> T {
        v[self.below(v.len() as u64) as usize]
    }
}

// ── comparing against the oracle ─────────────────────────────────────────────

fn map_color(c: ColorType) -> Option<PixelFormat> {
    Some(match c {
        ColorType::L8 => PixelFormat::L8,
        ColorType::La8 => PixelFormat::La8,
        ColorType::Rgb8 => PixelFormat::Rgb8,
        ColorType::Rgba8 => PixelFormat::Rgba8,
        ColorType::L16 => PixelFormat::L16,
        ColorType::La16 => PixelFormat::La16,
        ColorType::Rgb16 => PixelFormat::Rgb16,
        ColorType::Rgba16 => PixelFormat::Rgba16,
        ColorType::Rgb32F => PixelFormat::Rgb32F,
        ColorType::Rgba32F => PixelFormat::Rgba32F,
        _ => return None,
    })
}

#[derive(Default, Debug)]
struct Tally {
    identical: usize,
    both_refused: usize,
    /// We decode, the oracle refuses (a superset; listed, not failed).
    ours_only: usize,
    mismatched: Vec<String>,
}

impl Tally {
    fn check(
        &mut self,
        name: &str,
        ours: rusty_image::Result<Image>,
        theirs: image::ImageResult<DynamicImage>,
    ) {
        match (ours, theirs) {
            (Ok(a), Ok(b)) => {
                let same = map_color(b.color()) == Some(a.format)
                    && a.width == b.width()
                    && a.height == b.height()
                    && a.pixels == b.as_bytes();
                if same {
                    self.identical += 1;
                } else {
                    let first = a.pixels.iter().zip(b.as_bytes()).position(|(x, y)| x != y);
                    self.mismatched.push(format!(
                        "{name}: ours {}x{} {:?} vs image {}x{} {:?}, first diff at {first:?}",
                        a.width,
                        a.height,
                        a.format,
                        b.width(),
                        b.height(),
                        b.color()
                    ));
                }
            }
            (Err(_), Err(_)) => self.both_refused += 1,
            (Ok(_), Err(_)) => self.ours_only += 1,
            (Err(e), Ok(_)) => self
                .mismatched
                .push(format!("{name}: we refuse ({e}), image decodes")),
        }
    }

    fn report(&self, what: &str) {
        println!(
            "{what}: {} identical, {} refused by both, {} decoded by us only, {} mismatched",
            self.identical,
            self.both_refused,
            self.ours_only,
            self.mismatched.len()
        );
        for m in self.mismatched.iter().take(20) {
            println!("  {m}");
        }
        assert!(
            self.mismatched.is_empty(),
            "{what}: {} mismatches",
            self.mismatched.len()
        );
    }
}

fn oracle(data: &[u8], f: ImageFormat) -> image::ImageResult<DynamicImage> {
    image::load_from_memory_with_format(data, f)
}

/// Truncations and byte flips of a valid file: none may panic.
fn hammer(data: &[u8], rng: &mut Rng, decode: impl Fn(&[u8])) -> usize {
    let mut n = 0;
    for cut in [0, 1, 2, 13, 14, 17, 18, 30, 54] {
        if cut < data.len() {
            decode(&data[..cut]);
            n += 1;
        }
    }
    for _ in 0..4 {
        let cut = rng.below(data.len() as u64) as usize;
        decode(&data[..cut]);
        let mut m = data.to_vec();
        for _ in 0..1 + rng.below(4) {
            let i = rng.below(m.len() as u64) as usize;
            m[i] ^= 1 << rng.below(8);
        }
        decode(&m);
        n += 2;
    }
    n
}

// ── BMP ──────────────────────────────────────────────────────────────────────

/// A random RLE stream for a `w` x `h` image (valid structure, random
/// contents: runs, absolute runs, deltas, end-of-line, end-of-file).
fn rle_stream(rng: &mut Rng, w: usize, h: usize, rle4: bool) -> Vec<u8> {
    let mut s = Vec::new();
    let (mut x, mut y) = (0usize, 0usize);
    while y < h {
        match rng.below(10) {
            0..=3 if x < w => {
                let n = 1 + rng.below((w - x).min(255) as u64) as usize;
                s.push(n as u8);
                s.push(rng.byte());
                x += n;
            }
            4..=5 if x + 3 <= w => {
                let n = 3 + rng.below((w - x - 2).min(253) as u64) as usize;
                s.push(0);
                s.push(n as u8);
                let mut len = if rle4 { n.div_ceil(2) } else { n };
                len += len & 1;
                s.extend(rng.bytes(len));
                x += n;
            }
            6 if rng.below(3) == 0 && y + 1 < h => {
                let dx = rng.below((w - x.min(w)) as u64 + 1) as u8;
                let dy = rng.below((h - y).min(3) as u64) as u8;
                s.extend_from_slice(&[0, 2, dx, dy]);
                x += usize::from(dx);
                y += usize::from(dy);
            }
            7 if rng.below(40) == 0 => {
                s.extend_from_slice(&[0, 1]);
                return s;
            }
            _ => {
                s.extend_from_slice(&[0, 0]);
                x = 0;
                y += 1;
            }
        }
        if x > w {
            x = w;
        }
    }
    s.extend_from_slice(&[0, 1]);
    s
}

struct BmpSpec {
    header: u32,
    width: i32,
    height: i32,
    bits: u16,
    compression: u32,
    colors_used: u32,
    masks: [u32; 4],
}

/// The DIB (header + masks + palette + pixels) of a random file.
fn dib(rng: &mut Rng, s: &BmpSpec) -> Vec<u8> {
    let mut d = Vec::new();
    d.extend_from_slice(&s.header.to_le_bytes());
    if s.header == 12 {
        d.extend_from_slice(&(s.width as u16).to_le_bytes());
        d.extend_from_slice(&(s.height as u16).to_le_bytes());
        d.extend_from_slice(&1u16.to_le_bytes());
        d.extend_from_slice(&s.bits.to_le_bytes());
    } else {
        d.extend_from_slice(&s.width.to_le_bytes());
        d.extend_from_slice(&s.height.to_le_bytes());
        d.extend_from_slice(&1u16.to_le_bytes());
        d.extend_from_slice(&s.bits.to_le_bytes());
        d.extend_from_slice(&s.compression.to_le_bytes());
        d.extend_from_slice(&[0u8; 12]);
        d.extend_from_slice(&s.colors_used.to_le_bytes());
        d.extend_from_slice(&0u32.to_le_bytes());
        // V2+: masks inside the header; V4/V5: the rest zero.
        let mut extra = Vec::new();
        for m in s.masks {
            extra.extend_from_slice(&m.to_le_bytes());
        }
        let inside = s.header as usize - 40;
        extra.resize(inside.max(16), 0);
        d.extend_from_slice(&extra[..inside]);
    }
    if s.compression == 3 && matches!(s.header, 40 | 108 | 124) {
        for m in &s.masks[..3] {
            d.extend_from_slice(&m.to_le_bytes());
        }
    }
    let w = s.width.unsigned_abs() as usize;
    let h = s.height.unsigned_abs() as usize;
    if matches!(s.bits, 1 | 2 | 4 | 8) {
        let n = if s.colors_used == 0 {
            1usize << s.bits
        } else {
            s.colors_used as usize
        };
        let per = if s.header == 12 { 3 } else { 4 };
        d.extend(rng.bytes(n * per));
    }
    match s.compression {
        1 => d.extend(rle_stream(rng, w, h, false)),
        2 => d.extend(rle_stream(rng, w, h, true)),
        _ => {
            let row = (usize::from(s.bits) * w).div_ceil(32) * 4;
            d.extend(rng.bytes(row * h));
        }
    }
    d
}

fn bmp_file(dib: &[u8], header: u32, palette_and_masks: usize) -> Vec<u8> {
    let offset = 14 + header as usize + palette_and_masks;
    let mut f = b"BM".to_vec();
    f.extend_from_slice(&((14 + dib.len()) as u32).to_le_bytes());
    f.extend_from_slice(&0u32.to_le_bytes());
    f.extend_from_slice(&(offset as u32).to_le_bytes());
    f.extend_from_slice(dib);
    f
}

fn random_bmp_spec(rng: &mut Rng) -> BmpSpec {
    let header = rng.pick(&[12u32, 40, 52, 56, 108, 124]);
    let width = 1 + rng.below(37) as i32;
    let mut height = 1 + rng.below(29) as i32;
    let (bits, compression) = if header == 12 {
        (rng.pick(&[1u16, 4, 8, 24]), 0)
    } else {
        rng.pick(&[
            (1u16, 0u32),
            (2, 0),
            (4, 0),
            (8, 0),
            (16, 0),
            (24, 0),
            (32, 0),
            (8, 1),
            (4, 2),
            (16, 3),
            (32, 3),
        ])
    };
    if header != 12 && matches!(compression, 0 | 3) && rng.below(3) == 0 {
        height = -height;
    }
    let colors_used = if matches!(bits, 1 | 2 | 4 | 8) && rng.below(3) == 0 {
        1 + rng.below(1 << bits) as u32
    } else {
        0
    };
    let masks = match (bits, rng.below(4)) {
        (16, 0) => [0xf800, 0x07e0, 0x001f, 0],
        (16, 1) => [0x0f00, 0x00f0, 0x000f, 0xf000],
        (16, _) => [0x7c00, 0x03e0, 0x001f, 0x8000],
        (32, 0) => [0x00ff_0000, 0x0000_ff00, 0x0000_00ff, 0xff00_0000],
        (32, 1) => [0xff00_0000, 0x00ff_0000, 0x0000_ff00, 0],
        (32, 2) => [0x3ff0_0000, 0x000f_fc00, 0x0000_03ff, 0xc000_0000],
        _ => [0x00ff_0000, 0x0000_ff00, 0x0000_00ff, 0],
    };
    BmpSpec {
        header,
        width,
        height,
        bits,
        compression,
        colors_used,
        masks,
    }
}

#[test]
fn bmp_matches_image() {
    let mut rng = Rng(0x5eed_b0b0);
    let mut t = Tally::default();
    let mut dib_t = Tally::default();
    let mut hammered = 0;
    for i in 0..4000 {
        let s = random_bmp_spec(&mut rng);
        let d = dib(&mut rng, &s);
        let palette = if matches!(s.bits, 1 | 2 | 4 | 8) {
            let n = if s.colors_used == 0 {
                1usize << s.bits
            } else {
                s.colors_used as usize
            };
            n * if s.header == 12 { 3 } else { 4 }
        } else {
            0
        };
        let masks = if s.compression == 3 && matches!(s.header, 40 | 108 | 124) {
            12
        } else {
            0
        };
        let f = bmp_file(&d, s.header, palette + masks);
        t.check(
            &format!("bmp#{i} h{} {}b c{}", s.header, s.bits, s.compression),
            rusty_image::bmp::decode(&f),
            oracle(&f, ImageFormat::Bmp),
        );
        // V4/V5 bitfields with the masks only inside the header (no copy
        // after it), as ImageMagick writes them: for small images the file
        // ends before header + 12.
        if masks == 12 && s.header != 40 {
            let h = s.header as usize;
            let d2 = [&d[..h], &d[h + 12..]].concat();
            let f2 = bmp_file(&d2, s.header, palette);
            t.check(
                &format!("bmp#{i} h{} masks in header only", s.header),
                rusty_image::bmp::decode(&f2),
                oracle(&f2, ImageFormat::Bmp),
            );
        }
        // The same DIB, headerless, as the clipboard hands it over.
        let theirs = image::codecs::bmp::BmpDecoder::new_without_file_header(Cursor::new(&d))
            .and_then(DynamicImage::from_decoder);
        dib_t.check(
            &format!("dib#{i}"),
            rusty_image::bmp::decode_dib(&d),
            theirs,
        );
        if i % 8 == 0 {
            hammered += hammer(&f, &mut rng, |b| {
                let _ = rusty_image::bmp::decode(b);
                let _ = rusty_image::decode(b);
            });
        }
    }
    t.report("BMP files");
    dib_t.report("headerless DIBs");
    println!("BMP: {hammered} truncated/flipped files, no panics");
}

#[test]
fn bmp_and_dib_round_trip() {
    let mut rng = Rng(7);
    for i in 0..300 {
        let (w, h) = (1 + rng.below(40) as u32, 1 + rng.below(40) as u32);
        let format = rng.pick(&[PixelFormat::L8, PixelFormat::Rgb8, PixelFormat::Rgba8]);
        let n = (w * h) as usize * format.bytes_per_pixel();
        let img = Image::new(w, h, format, rng.bytes(n)).unwrap();
        let f = rusty_image::encode(&img, Format::Bmp).unwrap();
        let back = rusty_image::decode(&f).unwrap();
        // L8 is stored as a grey palette, which BMP readers return as RGB.
        let want = if format == PixelFormat::L8 {
            img.pixels.iter().flat_map(|&v| [v, v, v]).collect()
        } else {
            img.pixels.clone()
        };
        assert_eq!(back.pixels, want, "bmp round trip #{i}");
        let theirs = oracle(&f, ImageFormat::Bmp).unwrap();
        assert_eq!(theirs.as_bytes(), &want[..], "image reads our bmp #{i}");
        if format == PixelFormat::Rgba8 {
            for kind in [
                rusty_image::bmp::DibKind::Dib,
                rusty_image::bmp::DibKind::DibV5,
            ] {
                let d = rusty_image::bmp::encode_dib(&img, kind).unwrap();
                let ours = rusty_image::bmp::decode_dib(&d).unwrap();
                let theirs =
                    image::codecs::bmp::BmpDecoder::new_without_file_header(Cursor::new(&d))
                        .and_then(DynamicImage::from_decoder)
                        .unwrap();
                assert_eq!(ours.pixels, theirs.as_bytes(), "{kind:?} #{i}");
                if kind == rusty_image::bmp::DibKind::DibV5 {
                    assert_eq!(ours.pixels, img.pixels, "CF_DIBV5 keeps alpha #{i}");
                }
            }
        }
    }
}

// ── TGA ──────────────────────────────────────────────────────────────────────

#[test]
fn tga_matches_image() {
    let mut rng = Rng(0x7a7a);
    let mut t = Tally::default();
    let mut hammered = 0;
    for i in 0..4000 {
        let image_type = rng.pick(&[1u8, 2, 3, 9, 10, 11]);
        let mapped = matches!(image_type, 1 | 9);
        let rle = image_type >= 9;
        let (depth, attr) = if mapped {
            (rng.pick(&[8u8, 16]), 0)
        } else if image_type == 3 || image_type == 11 {
            rng.pick(&[(8u8, 0u8), (16, 8)])
        } else {
            rng.pick(&[(15u8, 0u8), (16, 0), (16, 1), (24, 0), (32, 8), (32, 0)])
        };
        let map_bits = rng.pick(&[15u8, 16, 24, 32]);
        let map_attr = if map_bits == 32 {
            rng.pick(&[0u8, 8])
        } else {
            0
        };
        let map_len = if mapped {
            1 + rng.below(if depth == 8 { 256 } else { 600 }) as u16
        } else {
            0
        };
        let map_origin = if mapped { rng.below(3) as u16 } else { 0 };
        let (w, h) = (1 + rng.below(30) as u16, 1 + rng.below(30) as u16);
        let desc = (if mapped { map_attr } else { attr }) | (rng.below(4) as u8) << 4;
        let mut f = vec![0u8, u8::from(mapped), image_type];
        f.extend_from_slice(&map_origin.to_le_bytes());
        f.extend_from_slice(&map_len.to_le_bytes());
        f.push(if mapped { map_bits } else { 0 });
        f.extend_from_slice(&[0, 0, 0, 0]);
        f.extend_from_slice(&w.to_le_bytes());
        f.extend_from_slice(&h.to_le_bytes());
        f.push(depth);
        f.push(desc);
        if mapped {
            f.extend(rng.bytes(usize::from(map_bits).div_ceil(8) * usize::from(map_len)));
        }
        let bpp = usize::from(depth).div_ceil(8);
        let n = usize::from(w) * usize::from(h);
        // Indices mostly inside the map.
        let pixel = |rng: &mut Rng| -> Vec<u8> {
            if mapped {
                let idx = map_origin as u64 + rng.below(u64::from(map_len) + 1);
                let idx = idx as u16;
                idx.to_le_bytes()[..bpp].to_vec()
            } else {
                rng.bytes(bpp)
            }
        };
        if rle {
            let mut left = n;
            while left > 0 {
                let c = 1 + rng.below(left.min(128) as u64) as usize;
                if rng.below(2) == 0 {
                    f.push(0x80 | (c - 1) as u8);
                    f.extend(pixel(&mut rng));
                } else {
                    f.push((c - 1) as u8);
                    for _ in 0..c {
                        f.extend(pixel(&mut rng));
                    }
                }
                left -= c;
            }
        } else {
            for _ in 0..n {
                f.extend(pixel(&mut rng));
            }
        }
        t.check(
            &format!("tga#{i} t{image_type} d{depth}"),
            rusty_image::tga::decode(&f),
            oracle(&f, ImageFormat::Tga),
        );
        if i % 8 == 0 {
            hammered += hammer(&f, &mut rng, |b| {
                let _ = rusty_image::tga::decode(b);
            });
        }
    }
    t.report("TGA");
    println!("TGA: {hammered} truncated/flipped files, no panics");
}

// ── HDR ──────────────────────────────────────────────────────────────────────

#[test]
fn hdr_matches_image() {
    let mut rng = Rng(0x4d52);
    let mut t = Tally::default();
    for i in 0..600 {
        let (w, h) = (1 + rng.below(70) as usize, 1 + rng.below(20) as usize);
        let mut f =
            b"#?RADIANCE\n# made by the oracle\nFORMAT=32-bit_rle_rgbe\nEXPOSURE=1.5\n\n".to_vec();
        f.extend_from_slice(format!("-Y {h} +X {w}\n").as_bytes());
        for _ in 0..h {
            match rng.below(3) {
                0 if (8..0x8000).contains(&w) => {
                    f.extend_from_slice(&[2, 2, (w >> 8) as u8, w as u8]);
                    for _ in 0..4 {
                        let mut x = 0;
                        while x < w {
                            let n = 1 + rng.below((w - x).min(127) as u64) as usize;
                            if rng.below(2) == 0 {
                                f.push(128 + n as u8);
                                f.push(rng.byte());
                            } else {
                                f.push(n as u8);
                                f.extend(rng.bytes(n));
                            }
                            x += n;
                        }
                    }
                }
                1 => {
                    // Old RLE: a pixel, then (1,1,1,n) runs.
                    let first = [rng.byte() | 4, rng.byte(), rng.byte(), rng.byte()];
                    f.extend_from_slice(&first);
                    let mut x = 1;
                    let mut after_run = false;
                    while x < w {
                        // (Consecutive run markers multiply by 256; keep them apart.)
                        if !after_run && rng.below(3) == 0 {
                            after_run = true;
                            let n = 1 + rng.below((w - x).min(255) as u64) as usize;
                            f.extend_from_slice(&[1, 1, 1, n as u8]);
                            x += n;
                        } else {
                            after_run = false;
                            f.extend_from_slice(&[
                                rng.byte() | 4,
                                rng.byte(),
                                rng.byte(),
                                rng.byte(),
                            ]);
                            x += 1;
                        }
                    }
                }
                _ => {
                    for _ in 0..w {
                        f.extend_from_slice(&[rng.byte() | 4, rng.byte(), rng.byte(), rng.byte()]);
                    }
                }
            }
        }
        t.check(
            &format!("hdr#{i}"),
            rusty_image::hdr::decode(&f),
            oracle(&f, ImageFormat::Hdr),
        );
        let _ = hammer(&f, &mut rng, |b| {
            let _ = rusty_image::hdr::decode(b);
        });
    }
    t.report("HDR");
}

#[test]
fn hdr_orientations_and_round_trip() {
    // A 3x2 image written in all eight orientations decodes to the same pixels.
    let px = |x: usize, y: usize| [(x * 40 + 10) as u8, (y * 90 + 5) as u8, 7, 130];
    let (w, h) = (3usize, 2usize);
    let mut decoded = Vec::new();
    for (a1, a2) in [
        ("-Y", "+X"),
        ("-Y", "-X"),
        ("+Y", "+X"),
        ("+Y", "-X"),
        ("+X", "-Y"),
        ("+X", "+Y"),
        ("-X", "-Y"),
        ("-X", "+Y"),
    ] {
        let transposed = a1.ends_with('X');
        let (n1, n2) = if transposed { (w, h) } else { (h, w) };
        let mut f =
            format!("#?RADIANCE\nFORMAT=32-bit_rle_rgbe\n\n{a1} {n1} {a2} {n2}\n").into_bytes();
        for s in 0..n1 {
            for p in 0..n2 {
                let (x, y) = if transposed {
                    (
                        if a1 == "+X" { s } else { n1 - 1 - s },
                        if a2 == "-Y" { p } else { n2 - 1 - p },
                    )
                } else {
                    (
                        if a2 == "+X" { p } else { n2 - 1 - p },
                        if a1 == "-Y" { s } else { n1 - 1 - s },
                    )
                };
                f.extend_from_slice(&px(x, y));
            }
        }
        let img = rusty_image::hdr::decode(&f).unwrap();
        assert_eq!((img.width, img.height), (3, 2), "{a1} {a2}");
        decoded.push(img);
    }
    assert!(decoded.windows(2).all(|p| p[0] == p[1]));

    let mut rng = Rng(99);
    for _ in 0..50 {
        let (w, h) = (1 + rng.below(80) as u32, 1 + rng.below(10) as u32);
        let floats: Vec<f32> = (0..w * h * 3)
            .map(|_| rng.below(100_000) as f32 / 977.0)
            .collect();
        let bytes = floats.iter().flat_map(|v| v.to_ne_bytes()).collect();
        let img = Image::new(w, h, PixelFormat::Rgb32F, bytes).unwrap();
        let f = rusty_image::encode(&img, Format::Hdr).unwrap();
        let ours = rusty_image::decode(&f).unwrap();
        let theirs = oracle(&f, ImageFormat::Hdr).unwrap();
        assert_eq!(ours.pixels, theirs.as_bytes());
        // RGBE shares one exponent per pixel: each channel keeps 8 bits
        // relative to the pixel's largest channel.
        for (a, b) in floats
            .chunks_exact(3)
            .zip(ours.as_f32().unwrap().chunks_exact(3))
        {
            let max = a[0].max(a[1]).max(a[2]);
            for k in 0..3 {
                assert!(
                    (a[k] - b[k]).abs() <= max / 128.0 + 1e-6,
                    "RGBE: {a:?} vs {b:?}"
                );
            }
        }
    }
}

// ── ICO ──────────────────────────────────────────────────────────────────────

fn png_bytes(w: u32, h: u32, rgba: &[u8]) -> Vec<u8> {
    let img = image::RgbaImage::from_raw(w, h, rgba.to_vec()).unwrap();
    let mut out = Cursor::new(Vec::new());
    img.write_to(&mut out, ImageFormat::Png).unwrap();
    out.into_inner()
}

#[test]
fn ico_matches_image() {
    let mut rng = Rng(0x1c0);
    let mut t = Tally::default();
    for i in 0..800 {
        let n = 1 + rng.below(4) as usize;
        let mut bodies = Vec::new();
        let mut dirs = Vec::new();
        for _ in 0..n {
            let s = rng.pick(&[1u32, 7, 16, 24, 32, 48]);
            let (w, h) = (s, s);
            let body = match rng.below(4) {
                0 => {
                    let rgba = rng.bytes((w * h * 4) as usize);
                    png_bytes(w, h, &rgba)
                }
                k => {
                    let bits: u16 = [32, 24, 8][k as usize - 1];
                    let mut d = Vec::new();
                    d.extend_from_slice(&40u32.to_le_bytes());
                    d.extend_from_slice(&(w as i32).to_le_bytes());
                    d.extend_from_slice(&((h * 2) as i32).to_le_bytes());
                    d.extend_from_slice(&1u16.to_le_bytes());
                    d.extend_from_slice(&bits.to_le_bytes());
                    d.extend_from_slice(&[0u8; 24]);
                    if bits == 8 {
                        d.extend(rng.bytes(256 * 4));
                    }
                    let row = (usize::from(bits) * w as usize).div_ceil(32) * 4;
                    d.extend(rng.bytes(row * h as usize));
                    if rng.below(4) != 0 {
                        d.extend(rng.bytes((w as usize).div_ceil(32) * 4 * h as usize));
                    }
                    d
                }
            };
            dirs.push((w, h, rng.pick(&[0u16, 8, 24, 32])));
            bodies.push(body);
        }
        let mut f = vec![0, 0, 1, 0];
        f.extend_from_slice(&(n as u16).to_le_bytes());
        let mut off = 6 + 16 * n;
        for ((w, h, bpp), b) in dirs.iter().zip(&bodies) {
            f.extend_from_slice(&[*w as u8, *h as u8, 0, 0]);
            f.extend_from_slice(&1u16.to_le_bytes());
            f.extend_from_slice(&bpp.to_le_bytes());
            f.extend_from_slice(&(b.len() as u32).to_le_bytes());
            f.extend_from_slice(&(off as u32).to_le_bytes());
            off += b.len();
        }
        for b in &bodies {
            f.extend_from_slice(b);
        }
        t.check(
            &format!("ico#{i}"),
            rusty_image::ico::decode(&f),
            oracle(&f, ImageFormat::Ico),
        );
        assert_eq!(
            rusty_image::ico::decode_entries(&f).map(|e| e.len()).ok(),
            Some(n)
        );
        let _ = hammer(&f, &mut rng, |b| {
            let _ = rusty_image::ico::decode(b);
            let _ = rusty_image::ico::decode_entries(b);
        });
    }
    t.report("ICO");
}

#[test]
fn ico_cur_round_trip() {
    let mut rng = Rng(5);
    let sizes = [16u32, 32, 48, 256];
    let imgs: Vec<Image> = sizes
        .iter()
        .map(|&s| Image::new(s, s, PixelFormat::Rgba8, rng.bytes((s * s * 4) as usize)).unwrap())
        .collect();
    let f = rusty_image::ico::encode(&imgs).unwrap();
    let entries = rusty_image::ico::decode_entries(&f).unwrap();
    for (e, img) in entries.iter().zip(&imgs) {
        // The AND mask makes alpha-0 pixels transparent; everything else is exact.
        assert_eq!(e.image.pixels, img.pixels);
    }
    assert!(entries[3].png && !entries[0].png);
    let theirs = oracle(&f, ImageFormat::Ico).unwrap();
    assert_eq!(
        theirs.as_bytes(),
        &imgs[3].pixels[..],
        "image picks the 256 entry"
    );
    let cur = rusty_image::ico::encode_cur(&[(imgs[0].clone(), 3, 9)]).unwrap();
    assert_eq!(
        rusty_image::ico::decode_entries(&cur).unwrap()[0].hotspot,
        Some((3, 9))
    );
    assert_eq!(rusty_image::probe(&cur), Some((Format::Cur, 16, 16)));
}

// ── PNM ──────────────────────────────────────────────────────────────────────

#[test]
fn pnm_matches_image() {
    let mut rng = Rng(0x9d9);
    let mut t = Tally::default();
    for i in 0..3000 {
        let (w, h) = (1 + rng.below(25) as usize, 1 + rng.below(25) as usize);
        let magic = rng.pick(b"1234567");
        let maxval: u32 = rng.pick(&[1, 15, 100, 255, 256, 1000, 4095, 65535]);
        let comment = if rng.below(3) == 0 {
            "# a comment\n"
        } else {
            ""
        };
        let mut f = Vec::new();
        let (ch, max) = match magic {
            b'1' | b'4' => {
                f.extend_from_slice(format!("P{}\n{comment}{w} {h}\n", magic as char).as_bytes());
                (1, 1)
            }
            b'2' | b'5' | b'3' | b'6' => {
                let ch = if matches!(magic, b'2' | b'5') { 1 } else { 3 };
                f.extend_from_slice(
                    format!("P{}\n{comment}{w}\n{h} {maxval}\n", magic as char).as_bytes(),
                );
                (ch, maxval)
            }
            _ => {
                let (depth, tupl) = rng.pick(&[
                    (1, Some("GRAYSCALE")),
                    (2, Some("GRAYSCALE_ALPHA")),
                    (3, Some("RGB")),
                    (4, Some("RGB_ALPHA")),
                    (1, Some("BLACKANDWHITE")),
                    (2, Some("BLACKANDWHITE_ALPHA")),
                    (1, None),
                    (3, None),
                ]);
                let maxval = if tupl.is_some_and(|t| t.starts_with("BLACK")) {
                    1
                } else {
                    maxval
                };
                let tl = tupl.map(|t| format!("TUPLTYPE {t}\n")).unwrap_or_default();
                f.extend_from_slice(
                    format!("P7\n{comment}WIDTH {w}\nHEIGHT {h}\nDEPTH {depth}\nMAXVAL {maxval}\n{tl}ENDHDR\n")
                        .as_bytes(),
                );
                (depth, maxval)
            }
        };
        let n = w * h * ch;
        match magic {
            b'1' => {
                for _ in 0..n {
                    f.extend_from_slice(if rng.below(2) == 0 { b"0 " } else { b"1\n" });
                }
            }
            b'4' => f.extend(rng.bytes(w.div_ceil(8) * h)),
            b'2' | b'3' => {
                for _ in 0..n {
                    f.extend_from_slice(format!("{} ", rng.below(u64::from(max) + 1)).as_bytes());
                }
            }
            _ => {
                let wide = max > 255;
                for _ in 0..n {
                    let v = rng.below(u64::from(max) + 1) as u32;
                    if wide {
                        f.extend_from_slice(&(v as u16).to_be_bytes());
                    } else {
                        f.push(v as u8);
                    }
                }
            }
        }
        t.check(
            &format!("pnm#{i} P{} max{max}", magic as char),
            rusty_image::pnm::decode(&f),
            oracle(&f, ImageFormat::Pnm),
        );
        let _ = hammer(&f, &mut rng, |b| {
            let _ = rusty_image::pnm::decode(b);
        });
    }
    t.report("PNM");
}

// ── QOI and the encoders ─────────────────────────────────────────────────────

/// Images with runs, small steps and noise, to reach every QOI op.
fn test_image(rng: &mut Rng, format: PixelFormat) -> Image {
    let (w, h) = (1 + rng.below(60) as u32, 1 + rng.below(60) as u32);
    let ch = format.bytes_per_pixel();
    let mut px = vec![0u8; (w * h) as usize * ch];
    let mut cur = rng.bytes(ch);
    for p in px.chunks_exact_mut(ch) {
        match rng.below(6) {
            0 => cur = rng.bytes(ch),
            1 => cur
                .iter_mut()
                .for_each(|c| *c = c.wrapping_add(rng.below(3) as u8)),
            2 => cur
                .iter_mut()
                .for_each(|c| *c = c.wrapping_add(rng.below(40) as u8)),
            _ => {}
        }
        p.copy_from_slice(&cur);
    }
    Image::new(w, h, format, px).unwrap()
}

#[test]
fn qoi_and_encoders_round_trip_through_image() {
    let mut rng = Rng(0x901);
    let mut checked = 0;
    for i in 0..400 {
        for (format, file) in [
            (PixelFormat::Rgb8, Format::Qoi),
            (PixelFormat::Rgba8, Format::Qoi),
            (PixelFormat::L8, Format::Tga),
            (PixelFormat::La8, Format::Tga),
            (PixelFormat::Rgb8, Format::Tga),
            (PixelFormat::Rgba8, Format::Tga),
            (PixelFormat::L8, Format::Pnm),
            (PixelFormat::Rgb8, Format::Pnm),
            (PixelFormat::Rgba8, Format::Pnm),
            (PixelFormat::La8, Format::Pnm),
        ] {
            let img = test_image(&mut rng, format);
            let f = rusty_image::encode(&img, file).unwrap();
            let ours = rusty_image::decode_as(&f, file).unwrap();
            assert_eq!(ours, img, "{file:?} {format:?} #{i}");
            let fmt = match file {
                Format::Qoi => ImageFormat::Qoi,
                Format::Tga => ImageFormat::Tga,
                _ => ImageFormat::Pnm,
            };
            let theirs = oracle(&f, fmt).unwrap();
            assert_eq!(
                theirs.as_bytes(),
                &img.pixels[..],
                "image reads our {file:?} #{i}"
            );
            checked += 1;
            // And image's encoding of the same pixels decodes the same here.
            if file != Format::Tga || format != PixelFormat::La8 {
                let dynimg = match format {
                    PixelFormat::L8 => DynamicImage::ImageLuma8(
                        image::GrayImage::from_raw(img.width, img.height, img.pixels.clone())
                            .unwrap(),
                    ),
                    PixelFormat::La8 => DynamicImage::ImageLumaA8(
                        image::GrayAlphaImage::from_raw(img.width, img.height, img.pixels.clone())
                            .unwrap(),
                    ),
                    PixelFormat::Rgb8 => DynamicImage::ImageRgb8(
                        image::RgbImage::from_raw(img.width, img.height, img.pixels.clone())
                            .unwrap(),
                    ),
                    _ => DynamicImage::ImageRgba8(
                        image::RgbaImage::from_raw(img.width, img.height, img.pixels.clone())
                            .unwrap(),
                    ),
                };
                let mut buf = Cursor::new(Vec::new());
                if dynimg.write_to(&mut buf, fmt).is_ok() {
                    let theirs_file = buf.into_inner();
                    let ours = rusty_image::decode_as(&theirs_file, file).unwrap();
                    assert_eq!(ours.pixels, img.pixels, "we read image's {file:?} #{i}");
                }
            }
        }
        let wide = test_image(&mut rng, PixelFormat::Rgba8);
        let px16: Vec<u8> = wide
            .pixels
            .iter()
            .flat_map(|&v| ((u16::from(v) * 257) ^ 3).to_ne_bytes())
            .collect();
        let img16 = Image::new(wide.width, wide.height, PixelFormat::Rgba16, px16).unwrap();
        let f = rusty_image::encode(&img16, Format::Pnm).unwrap();
        assert_eq!(rusty_image::decode(&f).unwrap(), img16);
        assert_eq!(
            oracle(&f, ImageFormat::Pnm).unwrap().as_bytes(),
            &img16.pixels[..]
        );
    }
    println!("encoders: {checked} images round-tripped through rusty_image and image");
}

#[test]
fn qoi_matches_image_on_its_own_encodings() {
    let mut rng = Rng(0x0e1);
    let mut t = Tally::default();
    for i in 0..500 {
        let img = test_image(
            &mut rng,
            if i % 2 == 0 {
                PixelFormat::Rgb8
            } else {
                PixelFormat::Rgba8
            },
        );
        let f = rusty_image::encode(&img, Format::Qoi).unwrap();
        t.check(
            &format!("qoi#{i}"),
            rusty_image::qoi::decode(&f),
            oracle(&f, ImageFormat::Qoi),
        );
        let _ = hammer(&f, &mut rng, |b| {
            let _ = rusty_image::qoi::decode(b);
        });
    }
    t.report("QOI");
}

// ── probe ────────────────────────────────────────────────────────────────────

#[test]
fn probe_matches_imagesize() {
    let mut rng = Rng(0xb0be);
    let mut files: Vec<(String, Vec<u8>)> = Vec::new();
    for i in 0..60 {
        let img = test_image(&mut rng, PixelFormat::Rgba8);
        let rgb = Image::new(
            img.width,
            img.height,
            PixelFormat::Rgb8,
            img.pixels
                .chunks_exact(4)
                .flat_map(|p| [p[0], p[1], p[2]])
                .collect(),
        )
        .unwrap();
        for f in [
            Format::Bmp,
            Format::Tga,
            Format::Qoi,
            Format::Pnm,
            Format::Ico,
        ] {
            if let Ok(b) = rusty_image::encode(&img, f) {
                files.push((format!("{f:?}#{i}"), b));
            }
        }
        let floats: Vec<u8> = rgb
            .pixels
            .iter()
            .flat_map(|&v| (f32::from(v) / 10.0).to_ne_bytes())
            .collect();
        let hdr = Image::new(img.width, img.height, PixelFormat::Rgb32F, floats).unwrap();
        files.push((
            format!("hdr#{i}"),
            rusty_image::encode(&hdr, Format::Hdr).unwrap(),
        ));
        let dynimg = DynamicImage::ImageRgba8(
            image::RgbaImage::from_raw(img.width, img.height, img.pixels.clone()).unwrap(),
        );
        for f in [
            ImageFormat::Png,
            ImageFormat::Gif,
            ImageFormat::WebP,
            ImageFormat::Tiff,
        ] {
            let mut buf = Cursor::new(Vec::new());
            dynimg.write_to(&mut buf, f).unwrap();
            files.push((format!("{f:?}#{i}"), buf.into_inner()));
        }
        let mut buf = Cursor::new(Vec::new());
        DynamicImage::ImageRgb8(
            image::RgbImage::from_raw(rgb.width, rgb.height, rgb.pixels.clone()).unwrap(),
        )
        .write_to(&mut buf, ImageFormat::Jpeg)
        .unwrap();
        files.push((format!("jpeg#{i}"), buf.into_inner()));
    }
    let (mut agree, mut differ) = (0, Vec::new());
    for (name, b) in &files {
        let ours = rusty_image::probe(b).map(|(_, w, h)| (w as usize, h as usize));
        // imagesize has no answer for PAM (P7): the reference there is the
        // size a full decode produces. (Both used to say None: our probe
        // UTF-8-checked binary pixels after ENDHDR.)
        let theirs = imagesize::blob_size(b)
            .ok()
            .map(|s| (s.width, s.height))
            .or_else(|| {
                rusty_image::decode(b)
                    .ok()
                    .map(|i| (i.width as usize, i.height as usize))
            });
        if ours == theirs {
            agree += 1;
        } else {
            differ.push(format!("{name}: ours {ours:?} imagesize {theirs:?}"));
        }
    }
    println!("probe: {agree}/{} agree with imagesize", files.len());
    for d in &differ {
        println!("  {d}");
    }
    assert!(differ.is_empty());
}

#[test]
fn hostile_dimensions_error_before_allocating() {
    // A BMP claiming 65535 x 65535 x 4, a QOI claiming 4G x 4G, a PNM claiming
    // 2^31 x 2^31: refused without allocating.
    let mut bmp = b"BM".to_vec();
    bmp.extend_from_slice(&[0; 8]);
    bmp.extend_from_slice(&54u32.to_le_bytes());
    bmp.extend_from_slice(&40u32.to_le_bytes());
    bmp.extend_from_slice(&65535i32.to_le_bytes());
    bmp.extend_from_slice(&65535i32.to_le_bytes());
    bmp.extend_from_slice(&1u16.to_le_bytes());
    bmp.extend_from_slice(&32u16.to_le_bytes());
    bmp.extend_from_slice(&[0; 24]);
    assert!(rusty_image::decode(&bmp).is_err());
    let mut qoi = b"qoif".to_vec();
    qoi.extend_from_slice(&u32::MAX.to_be_bytes());
    qoi.extend_from_slice(&u32::MAX.to_be_bytes());
    qoi.extend_from_slice(&[4, 0]);
    assert!(rusty_image::decode(&qoi).is_err());
    assert!(rusty_image::decode(b"P5 2147483648 2147483648 255 ").is_err());
    assert!(rusty_image::decode(b"#?RADIANCE\n\n-Y 99999999 +X 99999999\n").is_err());
}

/// ArtCraft's report: a PAM whose pixels after ENDHDR are not UTF-8 (any
/// 16-bit PAM) probed as None while decoding fine.
#[test]
fn probe_reads_pam_with_binary_pixels() {
    let mut f =
        b"P7\nWIDTH 3\nHEIGHT 2\nDEPTH 1\nMAXVAL 65535\nTUPLTYPE GRAYSCALE\nENDHDR\n".to_vec();
    f.extend((0..6u16).flat_map(|i| (0xff00u16 | i).to_be_bytes()));
    let (_, w, h) = rusty_image::probe(&f).expect("probe");
    assert_eq!((w, h), (3, 2));
    let img = rusty_image::decode(&f).unwrap();
    assert_eq!((img.width, img.height), (3, 2));
}

/// New-style RLE scanlines under each of the eight orientation headers: the
/// same scanlines decode to the usual orientation's pixels, mirrored or
/// transposed. (`image` reads only `-Y +X`, so the reference here is our own
/// decode of that one, which the oracle test above pins to `image`.)
#[test]
fn hdr_rle_in_every_orientation() {
    let mut rng = Rng(0x0e1e);
    for (n1, n2) in [(5usize, 9usize), (7, 16), (3, 40), (2, 130)] {
        // Runs and noise, so both packet kinds occur.
        let mut floats = Vec::new();
        let mut cur = [0.5f32; 3];
        for _ in 0..n1 * n2 {
            if rng.below(3) == 0 {
                cur = [
                    rng.below(4000) as f32 / 7.0,
                    rng.below(300) as f32 / 900.0,
                    rng.below(90) as f32,
                ];
            }
            floats.extend(cur.iter().flat_map(|v| v.to_ne_bytes()));
        }
        let img = Image::new(n2 as u32, n1 as u32, PixelFormat::Rgb32F, floats).unwrap();
        let f = rusty_image::encode(&img, Format::Hdr).unwrap();
        let head = format!("#?RADIANCE\nFORMAT=32-bit_rle_rgbe\n\n-Y {n1} +X {n2}\n");
        assert!(f.starts_with(head.as_bytes()));
        let body = &f[head.len()..];
        assert_eq!(body[..2], [2, 2], "the encoder wrote new-style RLE");
        let usual = rusty_image::decode(&f).unwrap();
        for (a1, a2) in [
            ("-Y", "+X"),
            ("-Y", "-X"),
            ("+Y", "+X"),
            ("+Y", "-X"),
            ("+X", "-Y"),
            ("+X", "+Y"),
            ("-X", "-Y"),
            ("-X", "+Y"),
        ] {
            let mut g =
                format!("#?RADIANCE\nFORMAT=32-bit_rle_rgbe\n\n{a1} {n1} {a2} {n2}\n").into_bytes();
            g.extend_from_slice(body);
            let got = rusty_image::decode(&g).unwrap();
            let transposed = a1.ends_with('X');
            let (w, h) = if transposed { (n1, n2) } else { (n2, n1) };
            assert_eq!((got.width as usize, got.height as usize), (w, h));
            for s in 0..n1 {
                for p in 0..n2 {
                    let (x, y) = if transposed {
                        (
                            if a1 == "+X" { s } else { n1 - 1 - s },
                            if a2 == "-Y" { p } else { n2 - 1 - p },
                        )
                    } else {
                        (
                            if a2 == "+X" { p } else { n2 - 1 - p },
                            if a1 == "-Y" { s } else { n1 - 1 - s },
                        )
                    };
                    assert_eq!(
                        got.pixels[(y * w + x) * 12..][..12],
                        usual.pixels[(s * n2 + p) * 12..][..12],
                        "{a1} {a2}: scanline {s} pixel {p}"
                    );
                }
            }
        }
    }
}
