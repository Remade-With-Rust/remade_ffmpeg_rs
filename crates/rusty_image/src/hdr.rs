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

/// The scale of an exponent byte: `2^(e - 136)`, and zero for `e = 0`,
/// built from the same bit pattern and the same multiplication `image`
/// uses (all exact powers of two, so the table holds the same bits).
static EXP_SCALE: [f32; 256] = {
    let mut t = [0f32; 256];
    let mut e = 0;
    while e < 256 {
        t[e] = exp_scale(e as u8);
        e += 1;
    }
    t
};

/// `2^(e - 136)` as the `image` crate computes it (zero for `e = 0`).
const fn exp_scale(e: u8) -> f32 {
    f32::from_bits(if e > 1 {
        ((e - 1) as u32) << 23
    } else {
        (e as u32) << 22
    }) * 0.003_906_25
}

/// RGBE to linear float, exactly as the `image` crate computes it:
/// `2^(e - 136) * c`, with `e = 0` giving zero. The scale is one table load
/// (a compare, a select, a shift, a move to a float register and a
/// multiplication before).
fn to_f32(c: [u8; 3], e: u8) -> [f32; 3] {
    let exp = EXP_SCALE[usize::from(e)];
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
    // The four component rows of a new-style RLE scanline.
    let mut planes = vec![0u8; len * 4];
    let mut r = crate::read::Reader::new(data);
    r.seek(pos)?;
    // Where a scanline goes is settled once per scanline, not per pixel.
    // Not transposed: scanline `s` is output row `s` (`-Y`) or its mirror
    // from the bottom (`+Y`), filled left to right (`+X`) or right to left.
    // Transposed: it is output column `s` (`+X`) or its mirror (`-X`),
    // filled top to bottom (`-Y`) or bottom to top.
    let first_forward = matches!(a1, "-Y" | "+X");
    let second_forward = matches!(a2, "+X" | "-Y");
    let row_bytes = width as usize * 12;
    for s in 0..lines {
        let line = if first_forward { s } else { lines - 1 - s };
        if transposed {
            read_scanline(&mut r, &mut scan, &mut planes)?;
            let rows = img.pixels.chunks_exact_mut(row_bytes);
            let at = line * 12;
            if second_forward {
                for (row, px) in rows.zip(&scan) {
                    put_f32(&mut row[at..at + 12], px);
                }
            } else {
                for (row, px) in rows.rev().zip(&scan) {
                    put_f32(&mut row[at..at + 12], px);
                }
            }
            continue;
        }
        let row = &mut img.pixels[line * row_bytes..(line + 1) * row_bytes];
        if !second_forward {
            read_scanline(&mut r, &mut scan, &mut planes)?;
            for (d, px) in row.chunks_exact_mut(12).rev().zip(&scan) {
                put_f32(d, px);
            }
            continue;
        }
        let rest = r.rest();
        let new_rle = rest.len() >= 4 && rest[0] == 2 && rest[1] == 2 && rest[2] < 128;
        if !new_rle {
            // Flat / old-RLE scanline read left to right: convert straight
            // from the file bytes into the output row, with no intermediate
            // scanline copy. Measured +8.6M Ir (6%) but -10% wall (0.29 s ->
            // 0.26 s, 6/6 interleaved pairs): the store-then-reload through
            // the scanline buffer cost more than the extra instructions.
            // The clock wins.
            read_flat_f32(&mut r, row)?;
            continue;
        }
        // New-style RLE read left to right: the four component rows are
        // decoded and converted straight into the output row.
        r.skip(4)?;
        read_rle_planes(&mut r, &mut planes, len)?;
        let (red, rest) = planes.split_at(len);
        let (green, rest) = rest.split_at(len);
        let (blue, exp) = rest.split_at(len);
        let rgbe = red.iter().zip(green).zip(blue).zip(exp);
        for (d, (((&r, &g), &b), &e)) in row.chunks_exact_mut(12).zip(rgbe) {
            put_f32(d, &[r, g, b, e]);
        }
    }
    Ok((img, meta))
}

/// Store one RGBE pixel as three native-order floats in `d` (12 bytes).
#[inline(always)]
fn put_f32(d: &mut [u8], px: &[u8; 4]) {
    let f = to_f32([px[0], px[1], px[2]], px[3]);
    d[0..4].copy_from_slice(&f[0].to_ne_bytes());
    d[4..8].copy_from_slice(&f[1].to_ne_bytes());
    d[8..12].copy_from_slice(&f[2].to_ne_bytes());
}

/// One flat or old-RLE scanline of `row.len() / 12` pixels, decoded straight
/// into 12-byte float pixels (`row`). Same rules and errors as
/// [`read_scanline`]'s flat path; a run repeats the previous output pixel.
fn read_flat_f32(r: &mut crate::read::Reader, row: &mut [u8]) -> Result<()> {
    let width = row.len() / 12;
    let is_run =
        |c: &[u8; 4]| u32::from_le_bytes([c[0], c[1], c[2], c[3]]) & 0x00ff_ffff == 0x0001_0101;
    let data = r.rest();
    let (mut used, mut x, mut mult) = (0usize, 0usize, 1usize);
    while x < width {
        // The pixels up to the next run marker (in a flat file, the whole
        // scanline): output and input are walked in step, so a pixel is
        // the run test and the conversion, with no index arithmetic and no
        // range check of its own (41 Ir a pixel with them).
        let mut n = 0;
        let pixels = row[x * 12..].chunks_exact_mut(12);
        for (d, c) in pixels.zip(data[used..].chunks_exact(4)) {
            let c: &[u8; 4] = c.try_into().unwrap();
            if is_run(c) {
                break;
            }
            put_f32(d, c);
            n += 1;
        }
        if n > 0 {
            mult = 1;
            x += n;
            used += n * 4;
            if x == width {
                break;
            }
        }
        // A run marker, or the end of the data.
        let c = data.get(used..used + 4).ok_or(Error::Truncated)?;
        used += 4;
        if x == 0 {
            return Err(Error::invalid("hdr: scanline starts with a run"));
        }
        let n = usize::from(c[3])
            .checked_mul(mult)
            .ok_or_else(|| Error::invalid("hdr: run overflow"))?;
        mult = mult.saturating_mul(256);
        if x + n > width {
            return Err(Error::invalid("hdr: scanline overrun"));
        }
        let (done, rest) = row.split_at_mut(x * 12);
        let p = &done[(x - 1) * 12..];
        for d in rest[..n * 12].chunks_exact_mut(12) {
            d.copy_from_slice(p);
        }
        x += n;
    }
    r.skip(used)
}

/// The body of a new-style RLE scanline (after its four-byte header): the
/// red, green, blue and exponent rows of `width` bytes each, one after the
/// other in `planes`.
///
/// The format stores the scanline a component at a time, so each component
/// is decoded into a row of its own, where a literal is one copy and a run
/// one fill. Writing every byte straight to its pixel was a range-checked
/// store at a stride of four per byte (29 Ir a pixel).
fn read_rle_planes(r: &mut crate::read::Reader, planes: &mut [u8], width: usize) -> Result<()> {
    for plane in planes.chunks_exact_mut(width) {
        let mut x = 0;
        while x < width {
            let n = r.u8()?;
            if n <= 128 {
                let n = usize::from(n);
                if x + n > width {
                    return Err(Error::invalid("hdr: scanline overrun"));
                }
                plane[x..x + n].copy_from_slice(r.bytes(n)?);
                x += n;
            } else {
                let n = usize::from(n - 128);
                if x + n > width {
                    return Err(Error::invalid("hdr: scanline overrun"));
                }
                let v = r.u8()?;
                plane[x..x + n].fill(v);
                x += n;
            }
        }
    }
    Ok(())
}

/// One scanline of `buf.len()` RGBE pixels into `buf`. `planes` is scratch
/// space of four times that many bytes.
fn read_scanline(
    r: &mut crate::read::Reader,
    buf: &mut [[u8; 4]],
    planes: &mut [u8],
) -> Result<()> {
    let width = buf.len();
    let first = r.array::<4>()?;
    if first[0] == 2 && first[1] == 2 && first[2] < 128 {
        // New-style RLE: decode the four component rows, then interleave
        // them into pixels (for the mirrored and transposed orientations;
        // the usual one converts from the component rows directly).
        read_rle_planes(r, planes, width)?;
        let (red, rest) = planes.split_at(width);
        let (green, rest) = rest.split_at(width);
        let (blue, exp) = rest.split_at(width);
        let rgbe = red.iter().zip(green).zip(blue).zip(exp);
        for (px, (((&r, &g), &b), &e)) in buf.iter_mut().zip(rgbe) {
            *px = [r, g, b, e];
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
    // Every component is <= m and m * scale is in [128, 256), so the product
    // is below 256: the i32 conversion already lands in 0..=255 (negative
    // components are cut to 0 first) and needs no clamp.
    let q = |v: f32| (v.max(0.0) * scale) as i32 as u8;
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
            // (Four planes read in step: an index is the plain way to say it.)
            #[allow(clippy::needless_range_loop)]
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
            // A run of four starts here: one word compare against the byte
            // splatted four times, instead of looking ahead byte by byte.
            // More instructions than the old look-ahead (whose loop usually
            // exited after one compare) but branch-free per position: wall
            // 0.42 -> 0.33 s. Testing the next byte first to skip the word
            // build had the best Ir and the worst wall (0.52 s): that test
            // is the unpredictable branch.
            if let Some(w) = v.get(i..i + 4) {
                let w = u32::from_le_bytes([w[0], w[1], w[2], w[3]]);
                if w == u32::from(v[i]) * 0x0101_0101 {
                    break;
                }
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

#[cfg(test)]
mod tests {
    use super::{decode, exp_scale, EXP_SCALE};

    fn flat_file(w: usize, scanline: &[u8]) -> Vec<u8> {
        let mut f = format!("#?RADIANCE\nFORMAT=32-bit_rle_rgbe\n\n-Y 1 +X {w}\n").into_bytes();
        f.extend_from_slice(scanline);
        f
    }

    /// Old-style runs in a flat scanline: consecutive run markers multiply
    /// (the second counts 256s), a literal pixel resets that, and the
    /// errors (a scanline starting with a run, an overrun, truncation) hold.
    #[test]
    fn flat_scanline_runs_and_errors() {
        let (p, q) = ([9u8, 8, 7, 130], [1u8, 2, 3, 129]);
        let px = |c: [u8; 4]| -> Vec<u8> {
            super::to_f32([c[0], c[1], c[2]], c[3])
                .iter()
                .flat_map(|v| v.to_ne_bytes())
                .collect()
        };
        // p, run of 2, then run of 1 x 256: 259 pixels of p, then q, run 3.
        let mut line = [p, [1, 1, 1, 2], [1, 1, 1, 1], q, [1, 1, 1, 3]].concat();
        let img = decode(&flat_file(263, &line)).unwrap();
        let mut want = px(p).repeat(259);
        want.extend(px(q).repeat(4));
        assert_eq!(img.pixels, want);
        // One pixel short of what the runs write is an overrun; one pixel
        // more is truncated data.
        assert!(decode(&flat_file(262, &line)).is_err());
        assert!(decode(&flat_file(264, &line)).is_err());
        line.truncate(line.len() - 1);
        assert!(decode(&flat_file(263, &line)).is_err());
        assert!(decode(&flat_file(3, &[[1, 1, 1, 2], p].concat())).is_err());
        assert!(decode(&flat_file(2, &[])).is_err());
    }

    /// The const-evaluated exponent table holds the bits the run-time
    /// formula produces, for every exponent byte.
    #[test]
    fn exp_table_matches_run_time_formula() {
        for e in 0..=255u8 {
            let e = std::hint::black_box(e);
            let run_time = f32::from_bits(if e > 1 {
                u32::from(e - 1) << 23
            } else {
                u32::from(e) << 22
            }) * std::hint::black_box(0.003_906_25f32);
            assert_eq!(
                EXP_SCALE[usize::from(e)].to_bits(),
                run_time.to_bits(),
                "{e}"
            );
            assert_eq!(exp_scale(e).to_bits(), run_time.to_bits(), "{e}");
        }
    }
}
