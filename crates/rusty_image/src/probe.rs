//! Header-only probing: format, width and height from the first bytes.

use crate::Format;

fn be16(d: &[u8], o: usize) -> Option<u32> {
    d.get(o..o + 2)
        .map(|b| u32::from(u16::from_be_bytes([b[0], b[1]])))
}
fn le16(d: &[u8], o: usize) -> Option<u32> {
    d.get(o..o + 2)
        .map(|b| u32::from(u16::from_le_bytes([b[0], b[1]])))
}
fn be32(d: &[u8], o: usize) -> Option<u32> {
    d.get(o..o + 4)
        .map(|b| u32::from_be_bytes([b[0], b[1], b[2], b[3]]))
}
fn le32(d: &[u8], o: usize) -> Option<u32> {
    d.get(o..o + 4)
        .map(|b| u32::from_le_bytes([b[0], b[1], b[2], b[3]]))
}
fn le24(d: &[u8], o: usize) -> Option<u32> {
    d.get(o..o + 3)
        .map(|b| u32::from(b[0]) | u32::from(b[1]) << 8 | u32::from(b[2]) << 16)
}

/// Recognise a format by its signature (TGA by its footer or a strict
/// header check, since it has no signature).
pub(crate) fn sniff(d: &[u8]) -> Option<Format> {
    if d.starts_with(&[0x89, b'P', b'N', b'G', 0x0d, 0x0a, 0x1a, 0x0a]) {
        Some(Format::Png)
    } else if d.starts_with(&[0xff, 0xd8, 0xff]) {
        Some(Format::Jpeg)
    } else if d.starts_with(b"GIF87a") || d.starts_with(b"GIF89a") {
        Some(Format::Gif)
    } else if d.len() >= 12 && &d[..4] == b"RIFF" && &d[8..12] == b"WEBP" {
        Some(Format::WebP)
    } else if d.starts_with(b"II*\0")
        || d.starts_with(b"MM\0*")
        || d.starts_with(b"II+\0")
        || d.starts_with(b"MM\0+")
    {
        Some(Format::Tiff)
    } else if d.len() >= 12 && &d[4..8] == b"ftyp" && is_avif(d) {
        Some(Format::Avif)
    } else if d.starts_with(b"BM") {
        Some(Format::Bmp)
    } else if d.starts_with(b"qoif") {
        Some(Format::Qoi)
    } else if d.starts_with(b"#?RADIANCE") || d.starts_with(b"#?RGBE") {
        Some(Format::Hdr)
    } else if d.len() >= 2 && d[0] == b'P' && (b'1'..=b'7').contains(&d[1]) {
        Some(Format::Pnm)
    } else if d.len() >= 6 && d[..2] == [0, 0] && (d[2] == 1 || d[2] == 2) && d[3] == 0 {
        let count = le16(d, 4)?;
        (count > 0).then_some(if d[2] == 1 { Format::Ico } else { Format::Cur })
    } else if looks_like_tga(d) {
        Some(Format::Tga)
    } else {
        None
    }
}

fn is_avif(d: &[u8]) -> bool {
    let size = be32(d, 0).unwrap_or(0) as usize;
    let end = size.min(d.len());
    if end < 16 {
        return false;
    }
    // Major brand, then the compatible brands after the minor version.
    std::iter::once(&d[8..12])
        .chain(d[16..end].chunks_exact(4))
        .any(|b| b == b"avif" || b == b"avis")
}

/// TGA has no signature: take the 2.0 footer, or a header whose fields form
/// a consistent TGA (the same checks `imagesize` makes).
fn looks_like_tga(d: &[u8]) -> bool {
    if d.len() < 18 {
        return false;
    }
    let (map_type, image_type) = (d[1], d[2]);
    if !matches!(image_type, 1 | 2 | 3 | 9 | 10 | 11) || map_type >= 2 {
        return false;
    }
    if d.len() >= 26 && d.ends_with(b"TRUEVISION-XFILE.\0") {
        return true;
    }
    if matches!(image_type, 1 | 9) && map_type != 1 {
        return false;
    }
    let map_spec = le32(d, 3).unwrap_or(1);
    let map_bits = d[7];
    if map_type == 0 && (map_spec != 0 || map_bits != 0) {
        return false;
    }
    if map_type == 1 && !matches!(map_bits, 0 | 8 | 16 | 24 | 32) {
        return false;
    }
    let (depth, desc) = (d[16], d[17]);
    let alpha = desc & 0x0f;
    if desc & 0x10 != 0 || !matches!(depth, 8 | 16 | 24 | 32) {
        return false;
    }
    if (depth == 8 || depth == 24) && alpha != 0 {
        return false;
    }
    if depth == 16 && alpha > 1 {
        return false;
    }
    if depth == 32 && alpha != 0 && alpha != 8 {
        return false;
    }
    le16(d, 12).unwrap_or(0) > 0 && le16(d, 14).unwrap_or(0) > 0
}

/// The format, width and height of an image, from its header alone: BMP,
/// TGA, HDR, ICO/CUR, PNM, QOI, PNG, JPEG, GIF, WebP, TIFF and AVIF.
/// ICO/CUR report their largest entry, AVIF its largest `ispe` (rotated by
/// `irot`), as `imagesize` does. `None` when the format is unknown or the
/// header is cut short.
pub fn probe(d: &[u8]) -> Option<(Format, u32, u32)> {
    let format = sniff(d)?;
    let (w, h) = match format {
        Format::Png => (be32(d, 16)?, be32(d, 20)?),
        Format::Gif => (le16(d, 6)?, le16(d, 8)?),
        Format::Qoi => (be32(d, 4)?, be32(d, 8)?),
        Format::Bmp => {
            if le32(d, 14)? == 12 {
                (le16(d, 18)?, le16(d, 20)?)
            } else {
                let w = le32(d, 18)? as i32;
                let h = le32(d, 22)? as i32;
                (w.unsigned_abs(), h.unsigned_abs())
            }
        }
        Format::Tga => (le16(d, 12)?, le16(d, 14)?),
        Format::Ico | Format::Cur => {
            let n = le16(d, 4)? as usize;
            let mut best: Option<(u32, u32)> = None;
            for i in 0..n {
                let o = 6 + 16 * i;
                let (Some(&w), Some(&h)) = (d.get(o), d.get(o + 1)) else {
                    break;
                };
                let size = (
                    u32::from(w.wrapping_sub(1)) + 1,
                    u32::from(h.wrapping_sub(1)) + 1,
                );
                best = best.max(Some(size));
            }
            best?
        }
        Format::Jpeg => jpeg(d)?,
        Format::WebP => webp(d)?,
        Format::Tiff => tiff(d)?,
        Format::Avif => avif(d)?,
        Format::Hdr => hdr(d)?,
        Format::Pnm => pnm(d)?,
    };
    Some((format, w, h))
}

fn jpeg(d: &[u8]) -> Option<(u32, u32)> {
    let mut i = 2;
    loop {
        while *d.get(i)? != 0xff {
            i += 1;
        }
        while *d.get(i)? == 0xff {
            i += 1;
        }
        let m = *d.get(i)?;
        i += 1;
        if matches!(m, 0xd0..=0xd9 | 0x01) {
            continue;
        }
        let len = be16(d, i)? as usize;
        if matches!(m, 0xc0..=0xcf) && !matches!(m, 0xc4 | 0xc8 | 0xcc) {
            return Some((be16(d, i + 5)?, be16(d, i + 3)?));
        }
        if len < 2 {
            return None;
        }
        i += len;
    }
}

fn webp(d: &[u8]) -> Option<(u32, u32)> {
    match d.get(12..16)? {
        b"VP8 " => {
            if d.get(23..26)? != [0x9d, 0x01, 0x2a] {
                return None;
            }
            Some((le16(d, 26)? & 0x3fff, le16(d, 28)? & 0x3fff))
        }
        b"VP8L" => {
            if *d.get(20)? != 0x2f {
                return None;
            }
            let b = le32(d, 21)?;
            Some(((b & 0x3fff) + 1, ((b >> 14) & 0x3fff) + 1))
        }
        b"VP8X" => Some((le24(d, 24)? + 1, le24(d, 27)? + 1)),
        _ => None,
    }
}

fn tiff(d: &[u8]) -> Option<(u32, u32)> {
    let le = d[0] == b'I';
    let u16_at = |o: usize| if le { le16(d, o) } else { be16(d, o) };
    let u32_at = |o: usize| if le { le32(d, o) } else { be32(d, o) };
    let u64_at = |o: usize| -> Option<u64> {
        let b: [u8; 8] = d.get(o..o + 8)?.try_into().ok()?;
        Some(if le {
            u64::from_le_bytes(b)
        } else {
            u64::from_be_bytes(b)
        })
    };
    let big = u16_at(2)? == 43;
    let (ifd, count_len, entry_len, value_off) = if big {
        (usize::try_from(u64_at(8)?).ok()?, 8, 20, 12)
    } else {
        (u32_at(4)? as usize, 2, 12, 8)
    };
    let count = if big {
        usize::try_from(u64_at(ifd)?).ok()?
    } else {
        u16_at(ifd)? as usize
    };
    let (mut w, mut h) = (None, None);
    for k in 0..count.min(4096) {
        let e = ifd + count_len + k * entry_len;
        let tag = u16_at(e)?;
        let ty = u16_at(e + 2)?;
        let v = match ty {
            3 => u16_at(e + value_off)?,
            4 => u32_at(e + value_off)?,
            16 if big => u32::try_from(u64_at(e + value_off)?).ok()?,
            _ => continue,
        };
        match tag {
            256 => w = Some(v),
            257 => h = Some(v),
            _ => {}
        }
        if let (Some(w), Some(h)) = (w, h) {
            return Some((w, h));
        }
    }
    None
}

/// The children of an ISOBMFF box body: (type, body).
fn boxes(mut d: &[u8]) -> impl Iterator<Item = (&[u8], &[u8])> {
    std::iter::from_fn(move || {
        let size = be32(d, 0)? as usize;
        let ty = d.get(4..8)?;
        let (header, size) = match size {
            1 => (
                16,
                usize::try_from(u64::from_be_bytes(d.get(8..16)?.try_into().ok()?)).ok()?,
            ),
            0 => (8, d.len()),
            s => (8, s),
        };
        if size < header || size > d.len() {
            return None;
        }
        let body = &d[header..size];
        d = &d[size..];
        Some((ty, body))
    })
}

fn avif(d: &[u8]) -> Option<(u32, u32)> {
    let meta = boxes(d).find(|(t, _)| *t == b"meta")?.1.get(4..)?;
    let iprp = boxes(meta).find(|(t, _)| *t == b"iprp")?.1;
    let ipco = boxes(iprp).find(|(t, _)| *t == b"ipco")?.1;
    let (mut best, mut rot) = (None::<(u32, u32)>, 0u8);
    for (t, body) in boxes(ipco) {
        if t == b"ispe" {
            let (w, h) = (be32(body, 4)?, be32(body, 8)?);
            let area = |s: (u32, u32)| u64::from(s.0) * u64::from(s.1);
            if best.is_none_or(|b| area((w, h)) > area(b)) {
                best = Some((w, h));
            }
        } else if t == b"irot" {
            rot = *body.first()? & 3;
        }
    }
    let (w, h) = best?;
    Some(if rot == 1 || rot == 3 { (h, w) } else { (w, h) })
}

fn hdr(d: &[u8]) -> Option<(u32, u32)> {
    let text = d.get(..d.len().min(64 * 1024))?;
    let mut lines = text.split(|&b| b == b'\n');
    lines.next()?;
    for l in lines.by_ref() {
        if l.is_empty() {
            break;
        }
    }
    let dims = std::str::from_utf8(lines.next()?).ok()?;
    let p: Vec<&str> = dims.split_whitespace().collect();
    if p.len() != 4 {
        return None;
    }
    let (n1, n2): (u32, u32) = (p[1].parse().ok()?, p[3].parse().ok()?);
    Some(if p[0].ends_with('X') {
        (n1, n2)
    } else {
        (n2, n1)
    })
}

fn pnm(d: &[u8]) -> Option<(u32, u32)> {
    if d[1] == b'7' {
        let text = std::str::from_utf8(d.get(..d.len().min(4096))?).ok()?;
        let (mut w, mut h) = (None, None);
        for l in text.lines().skip(1) {
            let mut it = l.split_whitespace();
            match it.next() {
                Some("WIDTH") => w = it.next()?.parse().ok(),
                Some("HEIGHT") => h = it.next()?.parse().ok(),
                Some("ENDHDR") => break,
                _ => {}
            }
        }
        return Some((w?, h?));
    }
    let mut r = crate::read::Reader::new(d);
    r.skip(2).ok()?;
    let mut num = || -> Option<u32> {
        let mut v: Option<u32> = None;
        let mut comment = false;
        loop {
            let b = match r.u8() {
                Ok(b) => b,
                Err(_) => return v,
            };
            if comment {
                comment = b != b'\n' && b != b'\r';
                continue;
            }
            match b {
                b'#' => comment = true,
                b'0'..=b'9' => v = Some(v.unwrap_or(0).checked_mul(10)? + u32::from(b - b'0')),
                _ if v.is_some() => return v,
                _ => {}
            }
        }
    };
    Some((num()?, num()?))
}
