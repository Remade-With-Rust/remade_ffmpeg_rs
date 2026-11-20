//! Radiance HDR (RGBE).
//!
//! Decodes `32-bit_rle_rgbe` with new-style (per-component) and old-style
//! RLE, in every orientation the format allows (`-Y H +X W` is the usual
//! one; the other seven are transposed or flipped into it), to
//! [`PixelFormat::Rgb32F`]. The RGBE-to-float conversion is the `image`
//! crate's, so the floats match it bit for bit. `EXPOSURE` is reported by
//! [`decode_with_meta`], not applied.

use crate::{Error, Image, PixelFormat, Result};

/// Header facts beyond the pixels.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct Meta {
    /// The product of every `EXPOSURE=` line, if any.
    pub exposure: Option<f32>,
    /// The dimensions line, e.g. `-Y 512 +X 768`.
    pub orientation: String,
}

/// RGBE to linear float, exactly as the `image` crate computes it:
/// `2^(e - 136) * c`, with `e = 0` giving zero.
fn to_f32(c: [u8; 3], e: u8) -> [f32; 3] {
    let exp = f32::from_bits(if e > 1 {
        u32::from(e - 1) << 23
    } else {
        u32::from(e) << 22
    }) * 0.003_906_25;
    [
        exp * f32::from(c[0]),
        exp * f32::from(c[1]),
        exp * f32::from(c[2]),
    ]
}

/// Decode an HDR file.
pub fn decode(data: &[u8]) -> Result<Image> {
    decode_with_meta(data).map(|(img, _)| img)
}

/// Read one header line (without the newline); `None` at end of data.
fn line<'a>(data: &'a [u8], pos: &mut usize) -> Option<&'a [u8]> {
    if *pos >= data.len() {
        return None;
    }
    let rest = &data[*pos..];
    let end = rest.iter().position(|&b| b == b'\n').unwrap_or(rest.len());
    *pos += (end + 1).min(rest.len());
    Some(&rest[..end])
}

/// Decode an HDR file and its header facts.
pub fn decode_with_meta(data: &[u8]) -> Result<(Image, Meta)> {
    if !(data.starts_with(b"#?RADIANCE") || data.starts_with(b"#?RGBE")) {
        return Err(Error::invalid("hdr: no #?RADIANCE signature"));
    }
    let mut pos = 0;
    line(data, &mut pos);
    let mut meta = Meta::default();
    loop {
        let l = line(data, &mut pos).ok_or(Error::Truncated)?;
        if l.is_empty() {
            break;
        }
        if l[0] == b'#' {
            continue;
        }
        let l = String::from_utf8_lossy(l);
        if let Some(eq) = l.find('=').filter(|&p| p > 0 && p + 1 < l.len()) {
            let (key, val) = (l[..eq].trim(), &l[eq + 1..]);
            match key {
                "FORMAT" if val.trim() != "32-bit_rle_rgbe" => {
                    return Err(Error::unsupported(format!("hdr: FORMAT={}", val.trim())))
                }
                "EXPOSURE" => {
                    let v: f32 = val
                        .trim()
                        .parse()
                        .map_err(|_| Error::invalid("hdr: unparsable EXPOSURE"))?;
                    meta.exposure = Some(meta.exposure.unwrap_or(1.0) * v);
                }
                _ => {}
            }
        }
    }
    let dims = line(data, &mut pos).ok_or(Error::Truncated)?;
    let dims = String::from_utf8_lossy(dims).to_string();
    let parts: Vec<&str> = dims.split_whitespace().collect();
    if parts.len() != 4 {
        return Err(Error::invalid(format!("hdr: dimensions line {dims:?}")));
    }
    let n1: u32 = parts[1]
        .parse()
        .map_err(|_| Error::invalid("hdr: dimensions"))?;
    let n2: u32 = parts[3]
        .parse()
        .map_err(|_| Error::invalid("hdr: dimensions"))?;
    // The first axis advances per scanline, the second along one.
    let (a1, a2) = (parts[0], parts[2]);
    let valid = |a: &str| matches!(a, "-Y" | "+Y" | "-X" | "+X");
    if !valid(a1) || !valid(a2) || a1.as_bytes()[1] == a2.as_bytes()[1] {
        return Err(Error::invalid(format!("hdr: orientation {a1} {a2}")));
    }
    let transposed = a1.ends_with('X');
    let (width, height) = if transposed { (n1, n2) } else { (n2, n1) };
    meta.orientation = dims.trim().to_string();
    let mut img = Image::zeroed(width, height, PixelFormat::Rgb32F)?;
    if width == 0 || height == 0 {
        return Ok((img, meta));
    }
    let (lines, len) = (n1 as usize, n2 as usize);
    let mut scan = vec![[0u8; 4]; len];
    let mut r = crate::read::Reader::new(data);
    r.seek(pos)?;
    let standard = a1 == "-Y" && a2 == "+X";
    for s in 0..lines {
        read_scanline(&mut r, &mut scan)?;
        if standard {
            // Scanline s is output row s: one row slice, fixed 12-byte stores.
            let row = &mut img.pixels[s * len * 12..(s + 1) * len * 12];
            for (d, px) in row.chunks_exact_mut(12).zip(&scan) {
                let f = to_f32([px[0], px[1], px[2]], px[3]);
                d[0..4].copy_from_slice(&f[0].to_ne_bytes());
                d[4..8].copy_from_slice(&f[1].to_ne_bytes());
                d[8..12].copy_from_slice(&f[2].to_ne_bytes());
            }
            continue;
        }
        for (p, px) in scan.iter().enumerate() {
            let (x, y) = if transposed {
                (
                    if a1 == "+X" { s } else { lines - 1 - s },
                    if a2 == "-Y" { p } else { len - 1 - p },
                )
            } else {
                (
                    if a2 == "+X" { p } else { len - 1 - p },
                    if a1 == "-Y" { s } else { lines - 1 - s },
                )
            };
            let o = (y * width as usize + x) * 12;
            let f = to_f32([px[0], px[1], px[2]], px[3]);
            for (k, v) in f.iter().enumerate() {
                img.pixels[o + k * 4..o + k * 4 + 4].copy_from_slice(&v.to_ne_bytes());
            }
        }
    }
    Ok((img, meta))
}

fn read_scanline(r: &mut crate::read::Reader, buf: &mut [[u8; 4]]) -> Result<()> {
    let width = buf.len();
    let first = r.array::<4>()?;
    if first[0] == 2 && first[1] == 2 && first[2] < 128 {
        for c in 0..4 {
            let mut x = 0;
            while x < width {
                let n = r.u8()?;
                if n <= 128 {
                    let n = usize::from(n);
                    if x + n > width {
                        return Err(Error::invalid("hdr: scanline overrun"));
                    }
                    for (i, &v) in r.bytes(n)?.iter().enumerate() {
                        buf[x + i][c] = v;
                    }
                    x += n;
                } else {
                    let n = usize::from(n - 128);
                    if x + n > width {
                        return Err(Error::invalid("hdr: scanline overrun"));
                    }
                    let v = r.u8()?;
                    for p in &mut buf[x..x + n] {
                        p[c] = v;
                    }
                    x += n;
                }
            }
        }
        return Ok(());
    }
    // Old RLE: (1,1,1,n) repeats the previous pixel n times (x256 when
    // consecutive).
    let is_run = |p: [u8; 4]| p[0] == 1 && p[1] == 1 && p[2] == 1;
    if is_run(first) {
        return Err(Error::invalid("hdr: scanline starts with a run"));
    }
    buf[0] = first;
    // Walk the remaining bytes as 4-byte chunks and consume them once at the
    // end, instead of a fallible 4-byte read per pixel.
    let mut chunks = r.rest().chunks_exact(4);
    let mut used = 0usize;
    let (mut x, mut mult, mut prev) = (1usize, 1usize, first);
    while x < width {
        let c = chunks.next().ok_or(Error::Truncated)?;
        used += 4;
        let p = [c[0], c[1], c[2], c[3]];
        // `is_run` as one masked word compare.
        if u32::from_le_bytes(p) & 0x00ff_ffff == 0x0001_0101 {
            let n = usize::from(p[3])
                .checked_mul(mult)
                .ok_or_else(|| Error::invalid("hdr: run overflow"))?;
            mult = mult.saturating_mul(256);
            if x + n > width {
                return Err(Error::invalid("hdr: scanline overrun"));
            }
            buf[x..x + n].fill(prev);
            x += n;
        } else {
            mult = 1;
            prev = p;
            buf[x] = p;
            x += 1;
        }
    }
    r.skip(used)
}

/// Float RGB to RGBE (the standard `frexp` rounding Radiance uses).
#[inline]
fn to_rgbe(c: [f32; 3]) -> [u8; 4] {
    let m = c[0].max(c[1]).max(c[2]);
    if m.is_nan() || m <= 1e-32 || !m.is_finite() {
        return [0; 4];
    }
    // m = f * 2^e with f in [0.5, 1): frexp, read from the float's bits
    // (m > 1e-32 is a normal float, so its exponent field is the answer).
    let e = ((m.to_bits() >> 23) & 0xff) as i32 - 126;
    // 2^(8 - e), exact; 8 - e stays inside the normal exponent range for
    // the e values that reach here (-105 .. 129).
    let scale = f32::from_bits(((8 - e + 127).clamp(1, 254) as u32) << 23);
    let q = |v: f32| ((v.max(0.0) * scale) as i32).clamp(0, 255) as u8;
    let e = (e + 128).clamp(0, 255) as u8;
    [q(c[0]), q(c[1]), q(c[2]), e]
}

/// Encode an `Rgb32F` image as new-style RLE RGBE, `-Y H +X W`.
pub fn encode(image: &Image) -> Result<Vec<u8>> {
    if image.format != PixelFormat::Rgb32F {
        return Err(Error::unsupported(format!(
            "hdr: encoding {:?} (Rgb32F)",
            image.format
        )));
    }
    let (w, h) = (image.width as usize, image.height as usize);
    let mut out = format!(
        "#?RADIANCE\nFORMAT=32-bit_rle_rgbe\n\n-Y {} +X {}\n",
        image.height, image.width
    )
    .into_bytes();
    // One reused plane per RGBE component, filled straight from the pixel
    // bytes: no whole-image float copy, no per-row allocation.
    let mut planes = [vec![0u8; w], vec![0u8; w], vec![0u8; w], vec![0u8; w]];
    // Worst case is about one byte of RLE overhead per 128 literal bytes.
    out.reserve(w * h * 4 + h * (4 + 4 * (w / 128 + 1)));
    for row in image.pixels.chunks_exact(w * 12) {
        for (x, px) in row.chunks_exact(12).enumerate() {
            let f = |k: usize| f32::from_ne_bytes([px[k], px[k + 1], px[k + 2], px[k + 3]]);
            let e = to_rgbe([f(0), f(4), f(8)]);
            planes[0][x] = e[0];
            planes[1][x] = e[1];
            planes[2][x] = e[2];
            planes[3][x] = e[3];
        }
        if !(8..0x8000).contains(&w) {
            // Too short or too long for new-style RLE: flat pixels.
            for x in 0..w {
                out.extend_from_slice(&[planes[0][x], planes[1][x], planes[2][x], planes[3][x]]);
            }
            continue;
        }
        out.extend_from_slice(&[2, 2, (w >> 8) as u8, w as u8]);
        for plane in &planes {
            rle_component(plane, &mut out);
        }
    }
    Ok(out)
}

fn rle_component(v: &[u8], out: &mut Vec<u8>) {
    let mut i = 0;
    while i < v.len() {
        let mut run = 1;
        while i + run < v.len() && run < 127 && v[i + run] == v[i] {
            run += 1;
        }
        if run >= 4 {
            out.push(128 + run as u8);
            out.push(v[i]);
            i += run;
            continue;
        }
        let start = i;
        while i < v.len() && i - start < 128 {
            let mut ahead = 1;
            while i + ahead < v.len() && ahead < 4 && v[i + ahead] == v[i] {
                ahead += 1;
            }
            if ahead >= 4 {
                break;
            }
            i += 1;
        }
        out.push((i - start) as u8);
        if i - start <= 16 {
            // Short literal runs: inline bytes, not a memcpy call each.
            for &b in &v[start..i] {
                out.push(b);
            }
        } else {
            out.extend_from_slice(&v[start..i]);
        }
    }
}
