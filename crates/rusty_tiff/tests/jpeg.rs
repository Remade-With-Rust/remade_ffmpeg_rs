//! JPEG-in-TIFF (compression 7, shared JPEGTables): built by splitting
//! rusty_jpeg-encoded JPEGs into a tables stream and abbreviated chunks.
//!
//! Gates: (1) `read_image` (YCbCr converted to RGB) is byte-identical to
//! rusty_jpeg decoding the ORIGINAL complete JPEGs (proves the table splice
//! and chunk assembly); (2) `read_image_raw` (YCbCr left unconverted, which
//! is what image-tiff returns) is within a small tolerance of image-tiff,
//! whose JPEG decoder (zune-jpeg) has a different IDCT, so bit-exactness is
//! not expected there.
#![allow(clippy::pedantic)] // test harness code: oracle plumbing, not library API
#![cfg(feature = "jpeg")]

mod common;

/// Split a complete JPEG into (tables: SOI+DQT+DHT+EOI, abbreviated: rest).
fn split(jpeg: &[u8]) -> (Vec<u8>, Vec<u8>) {
    let mut tables = vec![0xFF, 0xD8];
    let mut abbr = vec![0xFF, 0xD8];
    let mut i = 2;
    loop {
        let m = jpeg[i + 1];
        if m == 0xDA {
            abbr.extend_from_slice(&jpeg[i..]);
            break;
        }
        let len = usize::from(u16::from_be_bytes([jpeg[i + 2], jpeg[i + 3]]));
        let seg = &jpeg[i..i + 2 + len];
        if m == 0xDB || m == 0xC4 {
            tables.extend_from_slice(seg);
        } else if m != 0xE0 {
            abbr.extend_from_slice(seg);
        }
        i += 2 + len;
    }
    tables.extend_from_slice(&[0xFF, 0xD9]);
    (tables, abbr)
}

/// Minimal LE TIFF writer for a JPEG-compressed image.
#[allow(clippy::too_many_arguments)]
fn tiff_with_jpeg(
    w: u32,
    h: u32,
    spp: u16,
    photo: u16,
    tile: Option<(u32, u32)>,
    tables: &[u8],
    chunks: &[Vec<u8>],
    rps: u32,
) -> Vec<u8> {
    let mut out = b"II\x2A\x00\0\0\0\0".to_vec();
    let mut offs = Vec::new();
    for c in chunks {
        offs.push(out.len() as u32);
        out.extend_from_slice(c);
    }
    let tables_off = out.len() as u32;
    out.extend_from_slice(tables);
    if out.len() % 2 == 1 {
        out.push(0);
    }
    let offs_off = out.len() as u32;
    offs.iter()
        .for_each(|o| out.extend_from_slice(&o.to_le_bytes()));
    let cnts_off = out.len() as u32;
    chunks
        .iter()
        .for_each(|c| out.extend_from_slice(&(c.len() as u32).to_le_bytes()));
    let bps_off = out.len() as u32;
    for _ in 0..spp {
        out.extend_from_slice(&8u16.to_le_bytes());
    }
    let n = chunks.len() as u32;
    let multi = |inline: u32, off: u32| if n == 1 { inline } else { off };
    let mut e: Vec<(u16, u16, u32, u32)> = vec![
        (256, 4, 1, w),
        (257, 4, 1, h),
        (258, 3, u32::from(spp), if spp == 1 { 8 } else { bps_off }),
        (259, 3, 1, 7),
        (262, 3, 1, u32::from(photo)),
        (277, 3, 1, u32::from(spp)),
        (347, 7, tables.len() as u32, tables_off),
    ];
    if let Some((tw, th)) = tile {
        e.push((322, 4, 1, tw));
        e.push((323, 4, 1, th));
        e.push((324, 4, n, multi(offs[0], offs_off)));
        e.push((325, 4, n, multi(chunks[0].len() as u32, cnts_off)));
    } else {
        e.push((273, 4, n, multi(offs[0], offs_off)));
        e.push((278, 4, 1, rps));
        e.push((279, 4, n, multi(chunks[0].len() as u32, cnts_off)));
    }
    if photo == 6 {
        // YCbCrSubSampling 1,1 as SHORT[2] inline (JPEG handles chroma itself).
        e.push((530, 3, 2, 0x0001_0001));
    }
    e.sort();
    let ifd = out.len() as u32;
    out[4..8].copy_from_slice(&ifd.to_le_bytes());
    out.extend_from_slice(&(e.len() as u16).to_le_bytes());
    for (t, ty, c, v) in e {
        out.extend_from_slice(&t.to_le_bytes());
        out.extend_from_slice(&ty.to_le_bytes());
        out.extend_from_slice(&c.to_le_bytes());
        if ty == 3 && c == 1 {
            out.extend_from_slice(&(v as u16).to_le_bytes());
            out.extend_from_slice(&[0, 0]);
        } else {
            out.extend_from_slice(&v.to_le_bytes());
        }
    }
    out.extend_from_slice(&[0; 4]);
    out
}

fn encode_jpeg(px: &[u8], w: u16, h: u16, ct: rusty_jpeg::ColorType) -> Vec<u8> {
    let mut v = Vec::new();
    let mut enc = rusty_jpeg::Encoder::new(&mut v, 85);
    enc.set_optimized_huffman_tables(false);
    enc.encode(px, w, h, ct).unwrap();
    v
}

fn decode_jpeg(j: &[u8], ycc: bool) -> Vec<u8> {
    let mut d = rusty_jpeg::Decoder::new(j);
    if ycc {
        d.set_color_transform(rusty_jpeg::ColorTransform::YCbCr);
    }
    d.decode().unwrap()
}

fn photo_image(w: usize, h: usize, spp: usize) -> Vec<u8> {
    (0..w * h * spp)
        .map(|i| {
            let (p, c) = (i / spp, i % spp);
            let (x, y) = (p % w, p / w);
            ((x * 3 + y * 2 + c * 50) % 256) as u8 ^ ((x / 8 + y / 8) % 2 * 40) as u8
        })
        .collect()
}

#[test]
fn jpeg_strips_and_tiles() {
    let mut report = Vec::new();
    for &(spp, photo, ct) in &[
        (3u16, 6u16, rusty_jpeg::ColorType::Rgb),
        (1, 1, rusty_jpeg::ColorType::Luma),
    ] {
        let (w, h) = (80u32, 56u32);
        let src = photo_image(w as usize, h as usize, usize::from(spp));
        let s = usize::from(spp);
        // Strips of 16 rows.
        let rps = 16u32;
        let mut chunks = Vec::new();
        let mut want = Vec::new();
        let mut tables = Vec::new();
        for y0 in (0..h).step_by(rps as usize) {
            let rows = rps.min(h - y0);
            let part = &src[(y0 * w) as usize * s..((y0 + rows) * w) as usize * s];
            let j = encode_jpeg(part, w as u16, rows as u16, ct);
            want.extend(decode_jpeg(&j, spp == 3));
            let (t, a) = split(&j);
            tables = t;
            chunks.push(a);
        }
        let file = tiff_with_jpeg(w, h, spp, photo, None, &tables, &chunks, rps);
        let ours = common::ours(&file).unwrap();
        assert_eq!(
            ours[0].bytes, want,
            "strips spp={spp}: differs from rusty_jpeg"
        );
        let theirs = common::theirs(&file).unwrap();
        let raw = common::ours_raw(&file).unwrap();
        let maxdiff = raw[0]
            .bytes
            .iter()
            .zip(&theirs[0].bytes)
            .map(|(a, b)| a.abs_diff(*b))
            .max()
            .unwrap();
        let mean = raw[0]
            .bytes
            .iter()
            .zip(&theirs[0].bytes)
            .map(|(a, b)| f64::from(a.abs_diff(*b)))
            .sum::<f64>()
            / raw[0].bytes.len() as f64;
        assert_eq!(theirs[0].bytes.len(), raw[0].bytes.len());
        report.push(format!(
            "strips spp={spp}: vs image-tiff max |diff| {maxdiff}, mean {mean:.3}"
        ));
        assert!(maxdiff <= 16 && mean < 1.0, "{maxdiff} {mean}");

        // 32x32 tiles (edge tiles padded by replicating the last pixel).
        let (tw, th) = (32u32, 32u32);
        let mut chunks = Vec::new();
        let mut want = vec![0u8; src.len()];
        for ty in 0..h.div_ceil(th) {
            for tx in 0..w.div_ceil(tw) {
                let mut tile = vec![0u8; (tw * th) as usize * s];
                for y in 0..th {
                    for x in 0..tw {
                        let sx = (tx * tw + x).min(w - 1);
                        let sy = (ty * th + y).min(h - 1);
                        let si = (sy * w + sx) as usize * s;
                        let di = (y * tw + x) as usize * s;
                        tile[di..di + s].copy_from_slice(&src[si..si + s]);
                    }
                }
                let j = encode_jpeg(&tile, tw as u16, th as u16, ct);
                let dec = decode_jpeg(&j, spp == 3);
                for y in 0..th.min(h - ty * th) {
                    for x in 0..tw.min(w - tx * tw) {
                        let di = ((ty * th + y) * w + tx * tw + x) as usize * s;
                        let si = (y * tw + x) as usize * s;
                        want[di..di + s].copy_from_slice(&dec[si..si + s]);
                    }
                }
                let (t, a) = split(&j);
                tables = t;
                chunks.push(a);
            }
        }
        let file = tiff_with_jpeg(w, h, spp, photo, Some((tw, th)), &tables, &chunks, 0);
        let ours = common::ours(&file).unwrap();
        assert_eq!(ours[0].bytes, want, "tiles spp={spp}");
        let theirs = common::theirs(&file).unwrap();
        let raw = common::ours_raw(&file).unwrap();
        let maxdiff = raw[0]
            .bytes
            .iter()
            .zip(&theirs[0].bytes)
            .map(|(a, b)| a.abs_diff(*b))
            .max()
            .unwrap();
        let mean = raw[0]
            .bytes
            .iter()
            .zip(&theirs[0].bytes)
            .map(|(a, b)| f64::from(a.abs_diff(*b)))
            .sum::<f64>()
            / raw[0].bytes.len() as f64;
        report.push(format!(
            "tiles spp={spp}: vs image-tiff max |diff| {maxdiff}, mean {mean:.3}"
        ));
        assert!(maxdiff <= 16 && mean < 1.0, "{maxdiff} {mean}");
    }
    println!("JPEG-in-TIFF: {}", report.join("; "));
}
