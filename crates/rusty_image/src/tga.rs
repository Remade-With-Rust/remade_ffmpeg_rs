//! Truevision TGA.
//!
//! Decodes true-colour (15/16/24/32-bit), grey (8-bit, with or without 8-bit
//! alpha) and colour-mapped images, raw or RLE (packets may cross rows), in
//! all four origins. Output layouts follow the `image` crate: 15/16-bit
//! colour expands to RGB (its top bit is an attribute, not alpha), a
//! colour-mapped image takes its map's layout.

use crate::read::Reader;
use crate::{Error, Image, PixelFormat, Result};

/// `(255 x) / 31` rounded, for x in 0..32.
static LUT5: [u8; 32] = [
    0, 8, 16, 25, 33, 41, 49, 58, 66, 74, 82, 90, 99, 107, 115, 123, 132, 140, 148, 156, 165, 173,
    181, 189, 197, 206, 214, 222, 230, 239, 247, 255,
];

/// A 15/16-bit pixel as B, G, R (swapped to RGB at the end, like the rest).
fn expand15(v: [u8; 2]) -> [u8; 3] {
    let v = u16::from_le_bytes(v);
    [
        LUT5[(v & 31) as usize],
        LUT5[((v >> 5) & 31) as usize],
        LUT5[((v >> 10) & 31) as usize],
    ]
}

/// Decode a TGA file.
pub fn decode(data: &[u8]) -> Result<Image> {
    let mut r = Reader::new(data);
    let id_len = r.u8()?;
    let map_type = r.u8()?;
    let image_type = r.u8()?;
    let map_origin = r.u16le()? as usize;
    let map_len = r.u16le()? as usize;
    let map_entry_bits = r.u8()?;
    r.skip(4)?; // x/y origin
    let width = r.u16le()? as usize;
    let height = r.u16le()? as usize;
    let depth = r.u8()?;
    let desc = r.u8()?;

    let color_mapped = matches!(image_type, 1 | 9);
    let is_color = matches!(image_type, 1 | 2 | 9 | 10);
    let rle = matches!(image_type, 9..=11);
    let raw_bpp = usize::from(depth).div_ceil(8);
    let attr_bits = desc & 0x0f;

    if width == 0 || height == 0 {
        return Err(Error::invalid("tga: empty image"));
    }
    if color_mapped {
        if map_type != 1 {
            return Err(Error::invalid("tga: colour-mapped image without a map"));
        }
        if depth != 8 && depth != 16 {
            return Err(Error::invalid("tga: map indices must be 8 or 16-bit"));
        }
        if depth > map_entry_bits {
            return Err(Error::unsupported("tga: indices wider than map entries"));
        }
    }
    let total_bits = if color_mapped { map_entry_bits } else { depth };
    let other_bits = total_bits
        .checked_sub(attr_bits)
        .ok_or_else(|| Error::invalid("tga: more alpha bits than pixel bits"))?;
    let mut rgb15 = false;
    let format = match (attr_bits, other_bits, is_color) {
        (0, 32, true) | (8, 24, true) => PixelFormat::Rgba8,
        (0, 24, true) => PixelFormat::Rgb8,
        (1, 15, true) | (0, 15, true) | (0, 16, true) => {
            rgb15 = true;
            PixelFormat::Rgb8
        }
        (8, 8, false) => PixelFormat::La8,
        (0, 8, false) | (8, 0, false) => PixelFormat::L8,
        _ => {
            return Err(Error::unsupported(format!(
                "tga: {depth}-bit pixels with {attr_bits} alpha bits"
            )))
        }
    };
    r.skip(usize::from(id_len))?;

    let mut map: Option<(usize, Vec<u8>)> = None;
    if map_type == 1 {
        if ![15, 16, 24, 32].contains(&map_entry_bits) {
            return Err(Error::unsupported("tga: colour-map entry size"));
        }
        let entry = usize::from(map_entry_bits).div_ceil(8);
        let bytes = r.bytes(entry * map_len)?;
        if color_mapped {
            if entry == 2 {
                let expanded = bytes
                    .chunks_exact(2)
                    .flat_map(|c| expand15([c[0], c[1]]))
                    .collect();
                map = Some((3, expanded));
            } else {
                map = Some((entry, bytes.to_vec()));
            }
        }
    }

    let out_bpp = format.bytes_per_pixel();
    if raw_bpp > out_bpp {
        return Err(Error::unsupported(
            "tga: colour-mapped indices wider than the colour",
        ));
    }
    let mut img = Image::zeroed(width as u32, height as u32, format)?;
    let n_raw = width * height * raw_bpp;
    // Without a colour map or 15/16-bit expansion the raw pixels are the
    // output layout: decode straight into the output buffer.
    let direct = map.is_none() && !rgb15;
    let mut raw = if direct {
        std::mem::take(&mut img.pixels)
    } else {
        vec![0u8; n_raw]
    };
    if rle {
        let mut i = 0;
        while i < n_raw {
            let packet = r.u8()?;
            let count = usize::from(packet & 0x7f) + 1;
            if packet & 0x80 != 0 {
                let px = r.bytes(raw_bpp)?;
                for chunk in raw[i..].chunks_exact_mut(raw_bpp).take(count) {
                    chunk.copy_from_slice(px);
                }
                i += count * raw_bpp;
            } else {
                let n = (count * raw_bpp).min(n_raw - i);
                raw[i..i + n].copy_from_slice(r.bytes(n)?);
                i += n;
            }
        }
    } else {
        raw.copy_from_slice(r.bytes(n_raw)?);
    }

    // Orientation: bit 5 = top-to-bottom, bit 4 = right-to-left.
    let stride = width * raw_bpp;
    if desc & 0x20 == 0 && height > 1 {
        for y in 0..height / 2 {
            let (top, bottom) = raw.split_at_mut((height - 1 - y) * stride);
            top[y * stride..(y + 1) * stride].swap_with_slice(&mut bottom[..stride]);
        }
    }
    if desc & 0x10 != 0 && width > 1 {
        for row in raw.chunks_exact_mut(stride) {
            for x in 0..width / 2 {
                let (l, rr) = row.split_at_mut((width - 1 - x) * raw_bpp);
                l[x * raw_bpp..(x + 1) * raw_bpp].swap_with_slice(&mut rr[..raw_bpp]);
            }
        }
    }

    let out = &mut img.pixels;
    if let Some((entry, bytes)) = &map {
        let get = |i: usize| -> Result<&[u8]> {
            let at = entry * i.checked_sub(map_origin).ok_or_else(bad_index)?;
            bytes.get(at..at + entry).ok_or_else(bad_index)
        };
        if raw_bpp == 1 {
            for (&i, o) in raw.iter().zip(out.chunks_exact_mut(*entry)) {
                o.copy_from_slice(get(usize::from(i))?);
            }
        } else {
            for (i, o) in raw.chunks_exact(2).zip(out.chunks_exact_mut(*entry)) {
                o.copy_from_slice(get(usize::from(u16::from_le_bytes([i[0], i[1]])))?);
            }
        }
    } else if rgb15 {
        for (s, o) in raw.chunks_exact(2).zip(out.chunks_exact_mut(3)) {
            o.copy_from_slice(&expand15([s[0], s[1]]));
        }
    } else {
        *out = raw;
    }
    if matches!(format, PixelFormat::Rgb8 | PixelFormat::Rgba8) {
        for p in out.chunks_exact_mut(out_bpp) {
            p.swap(0, 2);
        }
    }
    Ok(img)
}

fn bad_index() -> Error {
    Error::invalid("tga: colour-map index out of range")
}

/// Encode `L8`, `La8`, `Rgb8` or `Rgba8` as an RLE TGA with a top-left
/// origin.
pub fn encode(image: &Image) -> Result<Vec<u8>> {
    let (image_type, depth, attr) = match image.format {
        PixelFormat::L8 => (11u8, 8u8, 0u8),
        PixelFormat::La8 => (11, 16, 8),
        PixelFormat::Rgb8 => (10, 24, 0),
        PixelFormat::Rgba8 => (10, 32, 8),
        other => {
            return Err(Error::unsupported(format!(
                "tga: encoding {other:?} (L8, La8, Rgb8, Rgba8)"
            )))
        }
    };
    if image.width == 0 || image.height == 0 || image.width > 0xffff || image.height > 0xffff {
        return Err(Error::unsupported("tga: 1 to 65535 pixels a side"));
    }
    let bpp = image.format.bytes_per_pixel();
    let mut out = vec![0u8, 0, image_type, 0, 0, 0, 0, 0, 0, 0, 0, 0];
    out.extend_from_slice(&(image.width as u16).to_le_bytes());
    out.extend_from_slice(&(image.height as u16).to_le_bytes());
    out.push(depth);
    out.push(attr | 0x20);
    // Stored pixels: BGR(A) for colour.
    let mut px = image.pixels.clone();
    if bpp >= 3 {
        for p in px.chunks_exact_mut(bpp) {
            p.swap(0, 2);
        }
    }
    // RLE per row (packets never cross rows, which every reader accepts).
    let w = image.width as usize;
    out.reserve(px.len() + px.len() / 128 + 1);
    for row in px.chunks_exact(w * bpp) {
        match bpp {
            1 => rle_row::<1>(row, &mut out),
            2 => rle_row::<2>(row, &mut out),
            3 => rle_row::<3>(row, &mut out),
            _ => rle_row::<4>(row, &mut out),
        }
    }
    // TGA 2.0 footer: no extension or developer area.
    out.extend_from_slice(&[0u8; 8]);
    out.extend_from_slice(b"TRUEVISION-XFILE.\0");
    Ok(out)
}

/// RLE-encode one row of `B`-byte pixels: runs of two or more identical
/// pixels as run packets, everything else as raw packets (a raw packet stops
/// where a run of two begins), each at most 128 pixels.
fn rle_row<const B: usize>(row: &[u8], out: &mut Vec<u8>) {
    let px = |i: usize| -> [u8; B] { row[i * B..i * B + B].try_into().unwrap() };
    let n = row.len() / B;
    let mut i = 0;
    while i < n {
        let p = px(i);
        let mut run = 1;
        while i + run < n && run < 128 && px(i + run) == p {
            run += 1;
        }
        if run > 1 {
            out.push(0x80 | (run - 1) as u8);
            out.extend_from_slice(&p);
            i += run;
            continue;
        }
        let start = i;
        i += 1;
        while i < n && i - start < 128 && !(i + 1 < n && px(i + 1) == px(i)) {
            i += 1;
        }
        out.push((i - start - 1) as u8);
        out.extend_from_slice(&row[start * B..i * B]);
    }
}
