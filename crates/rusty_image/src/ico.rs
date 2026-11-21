//! Windows icons and cursors (ICO / CUR).
//!
//! Each entry is a headerless BMP (with an AND mask) or a PNG. [`decode`]
//! returns the entry the `image` crate picks (highest bit depth, then
//! largest), so the two agree; [`decode_entries`] returns them all, with
//! CUR hotspots.

use crate::read::Reader;
use crate::{bmp, Error, Image, PixelFormat, Result};

const PNG_SIG: [u8; 8] = [0x89, b'P', b'N', b'G', 0x0d, 0x0a, 0x1a, 0x0a];

/// One directory entry.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct Dir {
    width: u8,
    height: u8,
    planes_or_x: u16,
    bpp_or_y: u16,
    length: u32,
    offset: u32,
}

impl Dir {
    fn real_w(&self) -> u32 {
        if self.width == 0 {
            256
        } else {
            u32::from(self.width)
        }
    }
    fn real_h(&self) -> u32 {
        if self.height == 0 {
            256
        } else {
            u32::from(self.height)
        }
    }
    fn matches(&self, w: u32, h: u32) -> bool {
        self.real_w() == w.min(256) && self.real_h() == h.min(256)
    }
}

/// A decoded entry.
#[derive(Clone, Debug, PartialEq)]
pub struct Entry {
    /// The pixels.
    pub image: Image,
    /// The cursor hotspot (x, y), for CUR files.
    pub hotspot: Option<(u16, u16)>,
    /// Whether the entry was stored as a PNG.
    pub png: bool,
}

fn read_dir(data: &[u8]) -> Result<(bool, Vec<Dir>)> {
    let mut r = Reader::new(data);
    let _reserved = r.u16le()?;
    let kind = r.u16le()?;
    let count = r.u16le()?;
    let mut dirs = Vec::with_capacity(usize::from(count));
    for _ in 0..count {
        let width = r.u8()?;
        let height = r.u8()?;
        let _colors = r.u8()?;
        let _reserved = r.u8()?;
        let planes_or_x = r.u16le()?;
        let bpp_or_y = r.u16le()?;
        if planes_or_x > 256 || bpp_or_y > 256 {
            return Err(Error::invalid("ico: planes or hotspot past 256"));
        }
        let length = r.u32le()?;
        let offset = r.u32le()?;
        dirs.push(Dir {
            width,
            height,
            planes_or_x,
            bpp_or_y,
            length,
            offset,
        });
    }
    Ok((kind == 2, dirs))
}

fn decode_entry(data: &[u8], d: &Dir) -> Result<(Image, bool)> {
    let start = d.offset as usize;
    let body = data.get(start..).ok_or(Error::Truncated)?;
    if body.len() < 8 {
        return Err(Error::Truncated);
    }
    if body[..8] == PNG_SIG {
        if d.length < 8 {
            return Err(Error::invalid("ico: PNG entry shorter than its signature"));
        }
        let img = decode_png(body)?;
        if !d.matches(img.width, img.height) {
            return Err(Error::invalid("ico: entry size differs from its PNG"));
        }
        return Ok((img, true));
    }
    let (mut img, end) = bmp::decode_ico_dib(body)?;
    if !d.matches(img.width, img.height) {
        return Err(Error::invalid("ico: entry size differs from its bitmap"));
    }
    let (w, h) = (img.width as usize, img.height as usize);
    let data_end = d.length as usize;
    let mask_row = w.div_ceil(32) * 4;
    let mask_len = mask_row * h;
    if data_end >= end + mask_len {
        let mask = body.get(end..end + mask_len).ok_or(Error::Truncated)?;
        // The mask is walked a byte (eight pixels) at a time, bottom row
        // first like the pixels. A zero byte, which is most of the mask of
        // an icon with real alpha, is skipped whole; the pixels of any other
        // byte have their alpha cleared without a branch (`bit - 1` is 0x00
        // for a set bit and 0xff for a clear one). Testing a bit and making
        // a bounds-checked store per pixel was 15 Ir a pixel.
        let rows = img.pixels.chunks_exact_mut(w * 4).rev();
        for (out, row) in rows.zip(mask.chunks_exact(mask_row)) {
            for (cell, &m) in out.chunks_mut(32).zip(row) {
                if m == 0 {
                    continue;
                }
                for (k, px) in cell.chunks_exact_mut(4).enumerate() {
                    px[3] &= ((m >> (7 - k)) & 1).wrapping_sub(1);
                }
            }
        }
    } else if data_end != end {
        return Err(Error::invalid(
            "ico: entry length disagrees with its bitmap",
        ));
    }
    Ok((img, false))
}

/// Decode the entry the `image` crate would pick: the highest bit depth,
/// then the largest area (later entries win ties lost by none).
pub fn decode(data: &[u8]) -> Result<Image> {
    let (_, mut dirs) = read_dir(data)?;
    let mut best = dirs
        .pop()
        .ok_or_else(|| Error::invalid("ico: no entries"))?;
    let score = |d: &Dir| (d.bpp_or_y, d.real_w() * d.real_h());
    let mut best_score = score(&best);
    for d in dirs {
        if score(&d) > best_score {
            best_score = score(&d);
            best = d;
        }
    }
    let (img, png) = decode_entry(data, &best)?;
    if png && img.format != PixelFormat::Rgba8 {
        return Err(Error::unsupported(
            "ico: PNG entries must be 8-bit RGBA (use decode_entries for others)",
        ));
    }
    Ok(img)
}

/// Decode every entry, in directory order. A CUR file's entries carry their
/// hotspots. PNG entries keep whatever layout the PNG has.
pub fn decode_entries(data: &[u8]) -> Result<Vec<Entry>> {
    let (cur, dirs) = read_dir(data)?;
    dirs.iter()
        .map(|d| {
            let (image, png) = decode_entry(data, d)?;
            Ok(Entry {
                image,
                hotspot: cur.then_some((d.planes_or_x, d.bpp_or_y)),
                png,
            })
        })
        .collect()
}

fn decode_png(data: &[u8]) -> Result<Image> {
    use rusty_png::{BitDepth, ColorType, Decoder, Transformations};
    let mut dec = Decoder::new(std::io::Cursor::new(data));
    dec.set_transformations(Transformations::EXPAND);
    let mut reader = dec
        .read_info()
        .map_err(|e| Error::invalid(format!("ico: PNG entry: {e}")))?;
    let mut buf = vec![0u8; reader.output_buffer_size()];
    let info = reader
        .next_frame(&mut buf)
        .map_err(|e| Error::invalid(format!("ico: PNG entry: {e}")))?;
    buf.truncate(info.buffer_size());
    let sixteen = info.bit_depth == BitDepth::Sixteen;
    let format = match (info.color_type, sixteen) {
        (ColorType::Grayscale, false) => PixelFormat::L8,
        (ColorType::GrayscaleAlpha, false) => PixelFormat::La8,
        (ColorType::Rgb, false) => PixelFormat::Rgb8,
        (ColorType::Rgba, false) => PixelFormat::Rgba8,
        (ColorType::Grayscale, true) => PixelFormat::L16,
        (ColorType::GrayscaleAlpha, true) => PixelFormat::La16,
        (ColorType::Rgb, true) => PixelFormat::Rgb16,
        (ColorType::Rgba, true) => PixelFormat::Rgba16,
        (c, _) => return Err(Error::unsupported(format!("ico: PNG colour type {c:?}"))),
    };
    if sixteen {
        // PNG is big-endian; Image samples are native.
        for c in buf.chunks_exact_mut(2) {
            let v = u16::from_be_bytes([c[0], c[1]]);
            c.copy_from_slice(&v.to_ne_bytes());
        }
    }
    Image::new(info.width, info.height, format, buf)
}

fn encode_png(image: &Image) -> Result<Vec<u8>> {
    use rusty_png::{BitDepth, ColorType, Encoder};
    let rgba = to_rgba8(image)?;
    let mut out = Vec::new();
    {
        let mut enc = Encoder::new(&mut out, image.width, image.height);
        enc.set_color(ColorType::Rgba);
        enc.set_depth(BitDepth::Eight);
        let mut w = enc
            .write_header()
            .map_err(|e| Error::invalid(format!("ico: PNG: {e}")))?;
        w.write_image_data(&rgba)
            .map_err(|e| Error::invalid(format!("ico: PNG: {e}")))?;
    }
    Ok(out)
}

fn to_rgba8(image: &Image) -> Result<Vec<u8>> {
    match image.format {
        PixelFormat::Rgba8 => Ok(image.pixels.clone()),
        PixelFormat::Rgb8 => Ok(image
            .pixels
            .chunks_exact(3)
            .flat_map(|p| [p[0], p[1], p[2], 0xff])
            .collect()),
        other => Err(Error::unsupported(format!(
            "ico: encoding {other:?} (Rgb8 and Rgba8)"
        ))),
    }
}

fn write(entries: &[(Image, u16, u16)], cur: bool) -> Result<Vec<u8>> {
    if entries.is_empty() || entries.len() > usize::from(u16::MAX) {
        return Err(Error::invalid("ico: 1 to 65535 entries"));
    }
    let mut bodies = Vec::new();
    for (img, _, _) in entries {
        if img.width == 0 || img.height == 0 || img.width > 256 || img.height > 256 {
            return Err(Error::unsupported(
                "ico: entries are 1 to 256 pixels a side",
            ));
        }
        to_rgba8(img)?;
        // 256-pixel entries as PNG (as Windows writes them), smaller as BMP.
        bodies.push(if img.width == 256 || img.height == 256 {
            encode_png(img)?
        } else {
            bmp::encode_ico_dib(img)?
        });
    }
    let mut out = Vec::new();
    out.extend_from_slice(&0u16.to_le_bytes());
    out.extend_from_slice(&(if cur { 2u16 } else { 1 }).to_le_bytes());
    out.extend_from_slice(&(entries.len() as u16).to_le_bytes());
    let mut offset = 6 + 16 * entries.len();
    for ((img, x, y), body) in entries.iter().zip(&bodies) {
        out.push(img.width as u8); // 256 -> 0
        out.push(img.height as u8);
        out.push(0);
        out.push(0);
        let (a, b) = if cur { (*x, *y) } else { (1, 32) };
        out.extend_from_slice(&a.to_le_bytes());
        out.extend_from_slice(&b.to_le_bytes());
        out.extend_from_slice(&(body.len() as u32).to_le_bytes());
        out.extend_from_slice(&(offset as u32).to_le_bytes());
        offset += body.len();
    }
    for b in bodies {
        out.extend_from_slice(&b);
    }
    Ok(out)
}

/// Encode an icon with one entry per image (`Rgb8` or `Rgba8`, 1 to 256
/// pixels a side): 256-pixel entries as PNG, smaller ones as 32-bit BMP
/// with an AND mask.
pub fn encode(images: &[Image]) -> Result<Vec<u8>> {
    let e: Vec<(Image, u16, u16)> = images.iter().map(|i| (i.clone(), 0, 0)).collect();
    write(&e, false)
}

/// Encode a cursor: each image with its hotspot (x, y).
pub fn encode_cur(entries: &[(Image, u16, u16)]) -> Result<Vec<u8>> {
    write(entries, true)
}
