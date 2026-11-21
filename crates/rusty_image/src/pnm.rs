//! Netpbm: PBM (P1/P4), PGM (P2/P5), PPM (P3/P6) and PAM (P7), ASCII and
//! binary, 8 and 16-bit.
//!
//! Samples are scaled to the full 8 or 16-bit range when `maxval` is not
//! 255 / 65535, with the `image` crate's float rounding, so the two agree
//! byte for byte. PBM decodes to `L8` with black 0 and white 255.

use crate::read::Reader;
use crate::{Error, Image, PixelFormat, Result};

fn is_space(b: u8) -> bool {
    matches!(b, b'\t' | b'\n' | 0x0b | 0x0c | b'\r' | b' ')
}

/// The next header integer: skips whitespace and `#` comments (to the end
/// of the line), then reads digits and the one whitespace byte after them.
fn next_u32(r: &mut Reader) -> Result<u32> {
    let mut value: u32 = 0;
    let mut found = false;
    let mut in_comment = false;
    while let Ok(b) = r.u8() {
        if in_comment {
            if b == b'\r' || b == b'\n' {
                in_comment = false;
            }
            continue;
        }
        if b == b'#' {
            in_comment = true;
            continue;
        }
        if is_space(b) {
            if found {
                break;
            }
            continue;
        }
        if !b.is_ascii_digit() {
            return Err(Error::invalid("pnm: bad digit in header"));
        }
        value = value
            .checked_mul(10)
            .and_then(|v| v.checked_add(u32::from(b - b'0')))
            .ok_or_else(|| Error::invalid("pnm: header number overflow"))?;
        found = true;
    }
    if found {
        Ok(value)
    } else {
        Err(Error::Truncated)
    }
}

/// One ASCII sample (for P1-P3), at most `max`.
fn ascii_sample(r: &mut Reader, max: u32) -> Result<u16> {
    let mut v: u16 = 0;
    let mut any = false;
    while let Ok(b) = r.u8() {
        if is_space(b) {
            if any {
                break;
            }
            continue;
        }
        if !b.is_ascii_digit() {
            return Err(Error::invalid("pnm: bad digit in raster"));
        }
        v = v
            .checked_mul(10)
            .and_then(|v| v.checked_add(u16::from(b - b'0')))
            .ok_or_else(|| Error::invalid("pnm: sample overflow"))?;
        any = true;
    }
    if !any {
        return Err(Error::Truncated);
    }
    if u32::from(v) > max {
        return Err(Error::invalid("pnm: sample overflow"));
    }
    Ok(v)
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum Tuple {
    Pbm,
    Bw,
    BwAlpha,
    Gray8,
    GrayA8,
    Gray16,
    GrayA16,
    Rgb8,
    Rgba8,
    Rgb16,
    Rgba16,
}

impl Tuple {
    fn format(self) -> PixelFormat {
        match self {
            Tuple::Pbm | Tuple::Bw | Tuple::Gray8 => PixelFormat::L8,
            Tuple::BwAlpha | Tuple::GrayA8 => PixelFormat::La8,
            Tuple::Gray16 => PixelFormat::L16,
            Tuple::GrayA16 => PixelFormat::La16,
            Tuple::Rgb8 => PixelFormat::Rgb8,
            Tuple::Rgba8 => PixelFormat::Rgba8,
            Tuple::Rgb16 => PixelFormat::Rgb16,
            Tuple::Rgba16 => PixelFormat::Rgba16,
        }
    }
}

fn by_maxval(maxval: u32, small: Tuple, big: Tuple) -> Result<Tuple> {
    match maxval {
        0 => Err(Error::invalid("pnm: maxval 0")),
        v if v <= 0xff => Ok(small),
        v if v <= 0xffff => Ok(big),
        v => Err(Error::invalid(format!("pnm: maxval {v}"))),
    }
}

struct Pam {
    width: u32,
    height: u32,
    depth: u32,
    maxval: u32,
    tupltype: Option<String>,
}

fn read_pam_header(r: &mut Reader) -> Result<Pam> {
    if r.u8()? != b'\n' {
        return Err(Error::invalid("pam: no newline after P7"));
    }
    let (mut w, mut h, mut d, mut m) = (None, None, None, None);
    let mut tupl: Option<String> = None;
    loop {
        let rest = r.rest();
        let end = rest.iter().position(|&b| b == b'\n').unwrap_or(rest.len());
        let line = std::str::from_utf8(&rest[..end])
            .map_err(|_| Error::invalid("pam: non-UTF-8 header"))?
            .to_string();
        r.skip((end + 1).min(rest.len()))?;
        if line.is_empty() {
            return Err(Error::invalid("pam: header ended without ENDHDR"));
        }
        if line.starts_with('#') {
            continue;
        }
        if !line.is_ascii() {
            return Err(Error::invalid("pam: non-ASCII header"));
        }
        let split = line.find(char::is_whitespace).unwrap_or(line.len());
        let trimmed = line.trim_start();
        let split = split.min(trimmed.len());
        let (key, val) = trimmed.split_at(split);
        let one = |slot: &mut Option<u32>| -> Result<()> {
            if slot.is_some() {
                return Err(Error::invalid(format!("pam: duplicate {key}")));
            }
            *slot = Some(
                val.trim()
                    .parse()
                    .map_err(|_| Error::invalid(format!("pam: {key} {val:?}")))?,
            );
            Ok(())
        };
        match key {
            "ENDHDR" => break,
            "WIDTH" => one(&mut w)?,
            "HEIGHT" => one(&mut h)?,
            "DEPTH" => one(&mut d)?,
            "MAXVAL" => one(&mut m)?,
            "TUPLTYPE" => {
                let t = val.trim();
                tupl = Some(match tupl {
                    Some(prev) => format!("{prev} {t}"),
                    None => t.to_string(),
                });
            }
            other => return Err(Error::invalid(format!("pam: header line {other:?}"))),
        }
    }
    match (w, h, d, m) {
        (Some(width), Some(height), Some(depth), Some(maxval)) => Ok(Pam {
            width,
            height,
            depth,
            maxval,
            tupltype: tupl,
        }),
        _ => Err(Error::invalid("pam: missing WIDTH/HEIGHT/DEPTH/MAXVAL")),
    }
}

fn pam_tuple(p: &Pam) -> Result<Tuple> {
    let (d, m) = (p.depth, p.maxval);
    if m == 0 {
        return Err(Error::invalid("pam: maxval 0"));
    }
    let bad = || Error::invalid(format!("pam: depth {d} / maxval {m} for its tuple type"));
    Ok(match p.tupltype.as_deref() {
        None if d == 1 => Tuple::Gray8,
        None if d == 2 => Tuple::GrayA8,
        None if d == 3 => Tuple::Rgb8,
        None if d == 4 => Tuple::Rgba8,
        None => return Err(Error::invalid("pam: unrecognised tuple type")),
        Some("BLACKANDWHITE") if m == 1 && d == 1 => Tuple::Bw,
        Some("BLACKANDWHITE") => return Err(bad()),
        Some("GRAYSCALE") if d == 1 && m <= 0xff => Tuple::Gray8,
        Some("GRAYSCALE") if d <= 1 && m <= 0xffff => Tuple::Gray16,
        Some("GRAYSCALE") => return Err(bad()),
        Some("RGB") if d == 3 && m <= 0xff => Tuple::Rgb8,
        Some("RGB") if d == 3 && m <= 0xffff => Tuple::Rgb16,
        Some("RGB") => return Err(bad()),
        Some("BLACKANDWHITE_ALPHA") if d == 2 && m == 1 => Tuple::BwAlpha,
        Some("BLACKANDWHITE_ALPHA") => return Err(bad()),
        Some("GRAYSCALE_ALPHA") if d == 2 && m <= 0xff => Tuple::GrayA8,
        Some("GRAYSCALE_ALPHA") if d == 2 && m <= 0xffff => Tuple::GrayA16,
        Some("GRAYSCALE_ALPHA") => return Err(bad()),
        Some("RGB_ALPHA") if d == 4 && m <= 0xff => Tuple::Rgba8,
        Some("RGB_ALPHA") if d == 4 && m <= 0xffff => Tuple::Rgba16,
        Some("RGB_ALPHA") => return Err(bad()),
        Some(other) => return Err(Error::unsupported(format!("pam: tuple type {other:?}"))),
    })
}

/// Decode a PBM, PGM, PPM or PAM file.
pub fn decode(data: &[u8]) -> Result<Image> {
    let mut r = Reader::new(data);
    let magic = r.array::<2>()?;
    if magic[0] != b'P' {
        return Err(Error::invalid("pnm: no P magic"));
    }
    let ascii = matches!(magic[1], b'1' | b'2' | b'3');
    let (width, height, maxval, tuple) = match magic[1] {
        b'1' | b'4' => {
            let w = next_u32(&mut r)?;
            let h = next_u32(&mut r)?;
            (w, h, 1, Tuple::Pbm)
        }
        b'2' | b'5' | b'3' | b'6' => {
            let w = next_u32(&mut r)?;
            let h = next_u32(&mut r)?;
            let m = next_u32(&mut r)?;
            let t = if matches!(magic[1], b'2' | b'5') {
                by_maxval(m, Tuple::Gray8, Tuple::Gray16)?
            } else {
                by_maxval(m, Tuple::Rgb8, Tuple::Rgb16)?
            };
            (w, h, m, t)
        }
        b'7' => {
            let p = read_pam_header(&mut r)?;
            let t = pam_tuple(&p)?;
            (p.width, p.height, p.maxval, t)
        }
        _ => return Err(Error::invalid("pnm: unknown magic")),
    };
    let format = tuple.format();
    let mut img = Image::zeroed(width, height, format)?;
    let (w, h) = (width as usize, height as usize);
    let buf = &mut img.pixels;
    let wide = format.bytes_per_sample() == 2;
    match (tuple, ascii) {
        (Tuple::Pbm, false) => {
            let line = w.div_ceil(8);
            let packed = r.bytes(line * h)?;
            for y in 0..h {
                for x in 0..w {
                    let bit = (packed[y * line + x / 8] >> (7 - x % 8)) & 1;
                    buf[y * w + x] = 1 - bit;
                }
            }
        }
        (Tuple::Pbm, true) => {
            for b in buf.iter_mut() {
                loop {
                    match r.u8()? {
                        c if is_space(c) => continue,
                        b'0' => *b = 255,
                        b'1' => *b = 0,
                        _ => return Err(Error::invalid("pbm: unexpected byte in raster")),
                    }
                    break;
                }
            }
        }
        (_, true) => {
            let max = if wide { 0xffff } else { 0xff };
            if wide {
                for c in buf.chunks_exact_mut(2) {
                    c.copy_from_slice(&ascii_sample(&mut r, max)?.to_ne_bytes());
                }
            } else {
                for b in buf.iter_mut() {
                    *b = ascii_sample(&mut r, max)? as u8;
                }
            }
        }
        (_, false) => {
            let n = buf.len();
            buf.copy_from_slice(r.bytes(n)?);
            if wide {
                for c in buf.chunks_exact_mut(2) {
                    let v = u16::from_be_bytes([c[0], c[1]]);
                    c.copy_from_slice(&v.to_ne_bytes());
                }
            }
            if matches!(tuple, Tuple::Bw | Tuple::BwAlpha) && buf.iter().any(|&v| v > 1) {
                return Err(Error::invalid("pam: black-and-white sample above 1"));
            }
        }
    }
    // Scale to the full range (with `image`'s f32 rounding).
    let target: u32 = if wide { 0xffff } else { 0xff };
    if maxval != target {
        let factor = target as f32 / maxval as f32;
        if wide {
            scale16(buf, factor);
        } else {
            scale8(buf, factor);
        }
    }
    Ok(img)
}

/// `(sample * factor).round()` capped at `cap` (an integer below 2^23),
/// without the libm call and without a float-to-integer conversion.
///
/// `f32::round` is a `roundf` call per sample on baseline x86-64 (20 of the
/// 33 instructions a sample cost), and Rust's saturating `as` cast does not
/// vectorise either, so both steps are done in float arithmetic that does:
///
/// - `(x + 2^23) - 2^23` is `x` rounded to the nearest integer, ties to
///   even (floats in `[2^23, 2^24)` are the integers), for `0 <= x < 2^23`;
/// - `x - r` is then exact, and rounding half away from zero differs from
///   ties-to-even only on a tie that went down, where `x - r == 0.5`;
/// - the integer `w` is read from the mantissa of `w + 2^23`.
///
/// Capping first changes nothing: `cap` is an integer, so anything above it
/// rounds to at least `cap`, where the cast to the sample type saturated.
#[inline(always)]
fn round_scaled(sample: f32, factor: f32, cap: f32) -> u32 {
    const TWO23: f32 = 8_388_608.0;
    let x = sample * factor;
    let x = if x > cap { cap } else { x };
    let r = (x + TWO23) - TWO23;
    let w = r + if x - r == 0.5 { 1.0 } else { 0.0 };
    (w + TWO23).to_bits() & 0x007f_ffff
}

/// Scale 8-bit samples by `factor` (255 / maxval), as `image` rounds them.
#[inline(never)]
fn scale8(buf: &mut [u8], factor: f32) {
    for v in buf.iter_mut() {
        *v = round_scaled(f32::from(*v), factor, 255.0) as u8;
    }
}

/// Scale native-order 16-bit samples by `factor` (65535 / maxval).
#[inline(never)]
fn scale16(buf: &mut [u8], factor: f32) {
    for c in buf.chunks_exact_mut(2) {
        let v = round_scaled(f32::from(u16::from_ne_bytes([c[0], c[1]])), factor, 65535.0);
        c.copy_from_slice(&(v as u16).to_ne_bytes());
    }
}

/// Encode as binary PGM (`L8`, `L16`), PPM (`Rgb8`, `Rgb16`) or PAM (the
/// alpha layouts), maxval 255 or 65535.
pub fn encode(image: &Image) -> Result<Vec<u8>> {
    let (w, h) = (image.width, image.height);
    let mut out = match image.format {
        PixelFormat::L8 => format!("P5\n{w} {h}\n255\n").into_bytes(),
        PixelFormat::L16 => format!("P5\n{w} {h}\n65535\n").into_bytes(),
        PixelFormat::Rgb8 => format!("P6\n{w} {h}\n255\n").into_bytes(),
        PixelFormat::Rgb16 => format!("P6\n{w} {h}\n65535\n").into_bytes(),
        f @ (PixelFormat::La8 | PixelFormat::Rgba8 | PixelFormat::La16 | PixelFormat::Rgba16) => {
            let (depth, tupl) = if f.channels() == 2 {
                (2, "GRAYSCALE_ALPHA")
            } else {
                (4, "RGB_ALPHA")
            };
            let max = if f.bytes_per_sample() == 2 {
                65535
            } else {
                255
            };
            format!(
                "P7\nWIDTH {w}\nHEIGHT {h}\nDEPTH {depth}\nMAXVAL {max}\nTUPLTYPE {tupl}\nENDHDR\n"
            )
            .into_bytes()
        }
        other => {
            return Err(Error::unsupported(format!(
                "pnm: encoding {other:?} (8 and 16-bit layouts)"
            )))
        }
    };
    if image.format.bytes_per_sample() == 2 {
        for c in image.pixels.chunks_exact(2) {
            out.extend_from_slice(&u16::from_ne_bytes([c[0], c[1]]).to_be_bytes());
        }
    } else {
        out.extend_from_slice(&image.pixels);
    }
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::round_scaled;

    /// The libm-free rounding is `f32::round` for every sample and maxval
    /// (all of them at 8 bits, every 16-bit sample for a spread of maxvals).
    #[test]
    fn scaling_matches_f32_round() {
        for maxval in 1..=255u32 {
            let factor = 255.0 / maxval as f32;
            for v in 0..=255u8 {
                let want = (f32::from(v) * factor).round() as u8;
                assert_eq!(
                    round_scaled(f32::from(v), factor, 255.0) as u8,
                    want,
                    "{v}/{maxval}"
                );
            }
        }
        let edges = [256u32, 257, 1023, 4095, 32767, 32768, 65534, 65535];
        for maxval in (256..=65535u32).step_by(211).chain(edges) {
            let factor = 65535.0 / maxval as f32;
            for v in 0..=65535u16 {
                let want = (f32::from(v) * factor).round() as u16;
                let got = round_scaled(f32::from(v), factor, 65535.0) as u16;
                assert_eq!(got, want, "{v}/{maxval}");
            }
        }
    }
}
