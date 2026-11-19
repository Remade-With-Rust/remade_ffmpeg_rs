//! QOI, the Quite OK Image format (specification 1.0).
//!
//! Decodes to `Rgb8` or `Rgba8` per the header's channel count, as the `qoi`
//! crate (which `image` uses) does; encodes `Rgb8` and `Rgba8`.

use crate::read::Reader;
use crate::{Error, Image, PixelFormat, Result};

const OP_INDEX: u8 = 0x00;
const OP_DIFF: u8 = 0x40;
const OP_LUMA: u8 = 0x80;
const OP_RUN: u8 = 0xc0;
const OP_RGB: u8 = 0xfe;
const OP_RGBA: u8 = 0xff;
const END: [u8; 8] = [0, 0, 0, 0, 0, 0, 0, 1];

fn hash(p: [u8; 4]) -> usize {
    (usize::from(p[0]) * 3 + usize::from(p[1]) * 5 + usize::from(p[2]) * 7 + usize::from(p[3]) * 11)
        % 64
}

/// Decode a QOI file.
pub fn decode(data: &[u8]) -> Result<Image> {
    let mut r = Reader::new(data);
    if r.array::<4>()? != *b"qoif" {
        return Err(Error::invalid("qoi: no qoif magic"));
    }
    let width = r.u32be()?;
    let height = r.u32be()?;
    let channels = r.u8()?;
    let colorspace = r.u8()?;
    if width == 0 || height == 0 {
        return Err(Error::invalid("qoi: empty image"));
    }
    if colorspace > 1 {
        return Err(Error::invalid("qoi: colour space"));
    }
    let format = match channels {
        3 => PixelFormat::Rgb8,
        4 => PixelFormat::Rgba8,
        n => return Err(Error::invalid(format!("qoi: {n} channels"))),
    };
    let len = crate::byte_len(width, height, format)?;
    let data = r.rest();
    let pixels = if channels == 3 {
        decode_ops::<3>(data, len)?
    } else {
        decode_ops::<4>(data, len)?
    };
    Image::new(width, height, format, pixels)
}

/// The op stream into `len` bytes of `CH`-channel pixels. Reads go through
/// `data.get`, so a cut-off stream is an error; the output is written
/// straight into a pre-sized buffer, one fixed-size store per pixel.
fn decode_ops<const CH: usize>(data: &[u8], len: usize) -> Result<Vec<u8>> {
    let mut out = vec![0u8; len];
    let mut index = [[0u8; 4]; 64];
    let mut px = [0u8, 0, 0, 255];
    let mut pos = 0usize;
    let mut o = 0usize;
    let byte = |pos: &mut usize| -> Result<u8> {
        let b = *data.get(*pos).ok_or(Error::Truncated)?;
        *pos += 1;
        Ok(b)
    };
    while o < len {
        let b = byte(&mut pos)?;
        let mut run = 1usize;
        match b {
            OP_RGB => {
                let c = data.get(pos..pos + 3).ok_or(Error::Truncated)?;
                px[..3].copy_from_slice(c);
                pos += 3;
            }
            OP_RGBA => {
                let c = data.get(pos..pos + 4).ok_or(Error::Truncated)?;
                px.copy_from_slice(c);
                pos += 4;
            }
            _ => match b & 0xc0 {
                OP_INDEX => px = index[usize::from(b & 0x3f)],
                OP_DIFF => {
                    px[0] = px[0].wrapping_add(((b >> 4) & 3).wrapping_sub(2));
                    px[1] = px[1].wrapping_add(((b >> 2) & 3).wrapping_sub(2));
                    px[2] = px[2].wrapping_add((b & 3).wrapping_sub(2));
                }
                OP_LUMA => {
                    let b2 = byte(&mut pos)?;
                    let dg = (b & 0x3f).wrapping_sub(32);
                    px[0] = px[0].wrapping_add(dg.wrapping_sub(8).wrapping_add(b2 >> 4));
                    px[1] = px[1].wrapping_add(dg);
                    px[2] = px[2].wrapping_add(dg.wrapping_sub(8).wrapping_add(b2 & 0x0f));
                }
                _ => run = usize::from(b & 0x3f) + 1,
            },
        }
        index[hash(px)] = px;
        let n = run.min((len - o) / CH);
        for chunk in out[o..o + n * CH].chunks_exact_mut(CH) {
            chunk.copy_from_slice(&px[..CH]);
        }
        o += n * CH;
    }
    Ok(out)
}

/// Encode `Rgb8` or `Rgba8` as QOI (sRGB colour space flag).
pub fn encode(image: &Image) -> Result<Vec<u8>> {
    let ch = match image.format {
        PixelFormat::Rgb8 => 3usize,
        PixelFormat::Rgba8 => 4,
        other => {
            return Err(Error::unsupported(format!(
                "qoi: encoding {other:?} (Rgb8, Rgba8)"
            )))
        }
    };
    if image.width == 0 || image.height == 0 {
        return Err(Error::unsupported("qoi: empty image"));
    }
    let mut out = Vec::with_capacity(14 + image.pixels.len() + 8);
    out.extend_from_slice(b"qoif");
    out.extend_from_slice(&image.width.to_be_bytes());
    out.extend_from_slice(&image.height.to_be_bytes());
    out.push(ch as u8);
    out.push(0);
    let mut index = [[0u8; 4]; 64];
    let mut prev = [0u8, 0, 0, 255];
    let mut run = 0u8;
    let n = image.pixels.len() / ch;
    for (i, p) in image.pixels.chunks_exact(ch).enumerate() {
        let px = [p[0], p[1], p[2], if ch == 4 { p[3] } else { 255 }];
        if px == prev {
            run += 1;
            if run == 62 || i == n - 1 {
                out.push(OP_RUN | (run - 1));
                run = 0;
            }
            continue;
        }
        if run > 0 {
            out.push(OP_RUN | (run - 1));
            run = 0;
        }
        let h = hash(px);
        if index[h] == px {
            out.push(OP_INDEX | h as u8);
        } else {
            index[h] = px;
            if px[3] == prev[3] {
                let dr = px[0].wrapping_sub(prev[0]) as i8;
                let dg = px[1].wrapping_sub(prev[1]) as i8;
                let db = px[2].wrapping_sub(prev[2]) as i8;
                let (dr_dg, db_dg) = (dr.wrapping_sub(dg), db.wrapping_sub(dg));
                if (-2..=1).contains(&dr) && (-2..=1).contains(&dg) && (-2..=1).contains(&db) {
                    out.push(
                        OP_DIFF | ((dr + 2) as u8) << 4 | ((dg + 2) as u8) << 2 | (db + 2) as u8,
                    );
                } else if (-32..=31).contains(&dg)
                    && (-8..=7).contains(&dr_dg)
                    && (-8..=7).contains(&db_dg)
                {
                    out.push(OP_LUMA | (dg + 32) as u8);
                    out.push(((dr_dg + 8) as u8) << 4 | (db_dg + 8) as u8);
                } else {
                    out.extend_from_slice(&[OP_RGB, px[0], px[1], px[2]]);
                }
            } else {
                out.extend_from_slice(&[OP_RGBA, px[0], px[1], px[2], px[3]]);
            }
        }
        prev = px;
    }
    out.extend_from_slice(&END);
    Ok(out)
}
