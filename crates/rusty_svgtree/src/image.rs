//! Embedded images: `data:` URI parsing, format sniffing, header-only size
//! probing at parse time, and bounded decoding at render time.

use rusty_raster::{IntSize, Pixmap, Size};

use crate::tree::ImageKind;
use crate::Options;

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum Format {
    Png,
    Jpeg,
}

fn sniff(data: &[u8]) -> Option<Format> {
    if data.starts_with(b"\x89PNG\r\n\x1a\n") {
        Some(Format::Png)
    } else if data.starts_with(&[0xff, 0xd8, 0xff]) {
        Some(Format::Jpeg)
    } else {
        None
    }
}

/// Standard and URL-safe base64, whitespace ignored, padding optional.
/// Stops at the first invalid byte.
// Out of line: the loops keep one layout whatever the caller looks like.
#[inline(never)]
pub(crate) fn base64_decode(text: &[u8]) -> Vec<u8> {
    let mut out = Vec::with_capacity(text.len() / 4 * 3 + 3);
    let mut acc: u32 = 0;
    let mut bits = 0u32;
    let mut rest = text;
    loop {
        if bits == 0 {
            // On a group boundary: whole groups of four digits, three
            // bytes each, for as long as they come.
            while let [a, b, c, d, tail @ ..] = rest {
                let (a, b, c, d) = (
                    B64[usize::from(*a)],
                    B64[usize::from(*b)],
                    B64[usize::from(*c)],
                    B64[usize::from(*d)],
                );
                if (a | b | c | d) >= B64_SKIP {
                    break;
                }
                let n = u32::from(a) << 18 | u32::from(b) << 12 | u32::from(c) << 6 | u32::from(d);
                out.extend_from_slice(&[(n >> 16) as u8, (n >> 8) as u8, n as u8]);
                rest = tail;
            }
        }
        // One character: a digit off a group boundary, white space, or the
        // end of the data.
        let [c, tail @ ..] = rest else {
            break;
        };
        rest = tail;
        let v = B64[usize::from(*c)];
        if v >= B64_SKIP {
            if v == B64_SKIP {
                continue;
            }
            break;
        }
        acc = (acc << 6) | u32::from(v);
        bits += 6;
        if bits >= 8 {
            bits -= 8;
            out.push((acc >> bits) as u8);
            acc &= (1 << bits) - 1;
        }
    }
    out
}

/// `B64` entry for white space (skipped).
const B64_SKIP: u8 = 0x40;
/// `B64` entry for `=` and anything else that ends the data.
const B64_END: u8 = 0x80;

/// Base64 digit values by byte (standard and URL-safe alphabets).
const B64: [u8; 256] = {
    let mut t = [B64_END; 256];
    let mut i = 0;
    while i < 26 {
        t[b'A' as usize + i] = i as u8;
        t[b'a' as usize + i] = i as u8 + 26;
        i += 1;
    }
    let mut i = 0;
    while i < 10 {
        t[b'0' as usize + i] = i as u8 + 52;
        i += 1;
    }
    t[b'+' as usize] = 62;
    t[b'-' as usize] = 62;
    t[b'/' as usize] = 63;
    t[b'_' as usize] = 63;
    t[b' ' as usize] = B64_SKIP;
    t[b'\t' as usize] = B64_SKIP;
    t[b'\r' as usize] = B64_SKIP;
    t[b'\n' as usize] = B64_SKIP;
    t[0x0c] = B64_SKIP;
    t
};

/// Percent-decoding for non-base64 data URIs.
fn percent_decode(text: &[u8]) -> Vec<u8> {
    let mut out = Vec::with_capacity(text.len());
    let mut i = 0;
    while i < text.len() {
        if text[i] == b'%' {
            let h = |c: u8| (c as char).to_digit(16);
            if let (Some(a), Some(b)) = (
                text.get(i + 1).and_then(|&c| h(c)),
                text.get(i + 2).and_then(|&c| h(c)),
            ) {
                out.push((a * 16 + b) as u8);
                i += 3;
                continue;
            }
        }
        out.push(text[i]);
        i += 1;
    }
    out
}

/// Resolves an `<image>` `href`. Only `data:` URIs are loaded (no file or
/// network access by design); PNG and JPEG are recognised by MIME type or
/// by content. Returns the encoded data and its intrinsic size, read from the
/// header only.
pub(crate) fn load_href(href: &str, opt: &Options) -> Option<(ImageKind, Size)> {
    let href = href.trim();
    if !crate::values::starts_with_ci(href, "data:") {
        return None;
    }
    let rest = &href[5..];
    let comma = rest.find(',')?;
    let (meta, payload) = (&rest[..comma], &rest[comma + 1..]);
    let is_b64 = meta
        .split(';')
        .any(|p| p.trim().eq_ignore_ascii_case("base64"));
    let mime = meta
        .split(';')
        .next()
        .unwrap_or("")
        .trim()
        .to_ascii_lowercase();
    let data = if is_b64 {
        base64_decode(payload.as_bytes())
    } else {
        percent_decode(payload.as_bytes())
    };
    let format = match mime.as_str() {
        "image/png" => Format::Png,
        "image/jpeg" | "image/jpg" => Format::Jpeg,
        "" | "text/plain" | "application/octet-stream" => sniff(&data)?,
        _ => return None,
    };
    let (w, h) = probe_size(format, &data)?;
    if u64::from(w) * u64::from(h) > opt.max_image_pixels {
        return None;
    }
    let size = Size::from_wh(w as f32, h as f32)?;
    let kind = match format {
        Format::Png => crate::convert::image_kind_png(data),
        Format::Jpeg => crate::convert::image_kind_jpeg(data),
    };
    Some((kind, size))
}

#[cfg(feature = "raster-images")]
fn probe_size(format: Format, data: &[u8]) -> Option<(u32, u32)> {
    match format {
        Format::Png => {
            let reader = rusty_png::Decoder::new(data).read_info().ok()?;
            let info = reader.info();
            Some((info.width, info.height))
        }
        Format::Jpeg => {
            let mut d = rusty_jpeg::Decoder::new(data);
            d.read_info().ok()?;
            let info = d.info()?;
            Some((u32::from(info.width), u32::from(info.height)))
        }
    }
}

#[cfg(not(feature = "raster-images"))]
fn probe_size(_format: Format, _data: &[u8]) -> Option<(u32, u32)> {
    None
}

fn premultiply_u8(c: u8, a: u8) -> u8 {
    // tiny-skia's `premultiply_u8`, so PNG alpha matches resvg exactly.
    let prod = u32::from(c) * u32::from(a) + 128;
    ((prod + (prod >> 8)) >> 8) as u8
}

/// Decodes an image into a premultiplied pixmap, refusing anything larger
/// than `max_pixels`.
#[cfg(feature = "raster-images")]
pub(crate) fn decode(kind: &ImageKind, max_pixels: u64) -> Option<Pixmap> {
    match kind {
        ImageKind::Png(data) => decode_png(data, max_pixels),
        ImageKind::Jpeg(data) => decode_jpeg(data, max_pixels),
    }
}

#[cfg(not(feature = "raster-images"))]
pub(crate) fn decode(_kind: &ImageKind, _max_pixels: u64) -> Option<Pixmap> {
    None
}

#[cfg(feature = "raster-images")]
fn decode_png(data: &[u8], max_pixels: u64) -> Option<Pixmap> {
    let mut decoder = rusty_png::Decoder::new(data);
    decoder.set_transformations(rusty_png::Transformations::normalize_to_color8());
    let mut reader = decoder.read_info().ok()?;
    {
        let info = reader.info();
        if u64::from(info.width) * u64::from(info.height) > max_pixels {
            return None;
        }
    }
    let mut buf = vec![0u8; reader.output_buffer_size()];
    let out = reader.next_frame(&mut buf).ok()?;
    if out.bit_depth != rusty_png::BitDepth::Eight {
        return None;
    }
    let size = IntSize::from_wh(out.width, out.height)?;
    let n = (out.width as usize).checked_mul(out.height as usize)?;
    let rgba = match out.color_type {
        // Opaque: nothing to premultiply (`premultiply_u8(c, 255) == c`).
        rusty_png::ColorType::Rgb => rgb_to_rgba(&buf, n)?,
        rusty_png::ColorType::Grayscale => gray_to_rgba(&buf, 1, n)?,
        rusty_png::ColorType::Rgba => {
            // Premultiplied where it was decoded: no second buffer.
            buf.truncate(n.checked_mul(4)?);
            if buf.len() != n * 4 {
                return None;
            }
            for p in buf.chunks_exact_mut(4) {
                let a = p[3];
                if a != 255 {
                    p[0] = premultiply_u8(p[0], a);
                    p[1] = premultiply_u8(p[1], a);
                    p[2] = premultiply_u8(p[2], a);
                }
            }
            buf
        }
        rusty_png::ColorType::GrayscaleAlpha => {
            let src = buf.get(..n.checked_mul(2)?)?;
            let mut rgba = vec![0u8; n.checked_mul(4)?];
            for (d, s) in rgba.chunks_exact_mut(4).zip(src.chunks_exact(2)) {
                let v = premultiply_u8(s[0], s[1]);
                d[0] = v;
                d[1] = v;
                d[2] = v;
                d[3] = s[1];
            }
            rgba
        }
        rusty_png::ColorType::Indexed => return None,
    };
    Pixmap::from_vec(rgba, size)
}

/// `n` opaque RGBA pixels from `n` RGB ones.
#[cfg(feature = "raster-images")]
fn rgb_to_rgba(src: &[u8], n: usize) -> Option<Vec<u8>> {
    let src = src.get(..n.checked_mul(3)?)?;
    let mut rgba = vec![255u8; n.checked_mul(4)?];
    for (d, s) in rgba.chunks_exact_mut(4).zip(src.chunks_exact(3)) {
        d[0] = s[0];
        d[1] = s[1];
        d[2] = s[2];
    }
    Some(rgba)
}

/// `n` opaque RGBA pixels from `n` grey ones of `step` bytes each (the first
/// byte is the level).
#[cfg(feature = "raster-images")]
fn gray_to_rgba(src: &[u8], step: usize, n: usize) -> Option<Vec<u8>> {
    let src = src.get(..n.checked_mul(step)?)?;
    let mut rgba = vec![255u8; n.checked_mul(4)?];
    for (d, s) in rgba.chunks_exact_mut(4).zip(src.chunks_exact(step)) {
        d[0] = s[0];
        d[1] = s[0];
        d[2] = s[0];
    }
    Some(rgba)
}

#[cfg(feature = "raster-images")]
fn decode_jpeg(data: &[u8], max_pixels: u64) -> Option<Pixmap> {
    let mut d = rusty_jpeg::Decoder::new(data);
    d.read_info().ok()?;
    let info = d.info()?;
    let (w, h) = (u32::from(info.width), u32::from(info.height));
    if u64::from(w) * u64::from(h) > max_pixels {
        return None;
    }
    d.set_max_decoding_buffer_size(
        usize::try_from(max_pixels.saturating_mul(4)).unwrap_or(usize::MAX),
    );
    let pixels = d.decode().ok()?;
    let n = (w as usize).checked_mul(h as usize)?;
    let rgba = match info.pixel_format {
        rusty_jpeg::PixelFormat::L8 => gray_to_rgba(&pixels, 1, n)?,
        rusty_jpeg::PixelFormat::L16 => gray_to_rgba(&pixels, 2, n)?,
        rusty_jpeg::PixelFormat::RGB24 => rgb_to_rgba(&pixels, n)?,
        rusty_jpeg::PixelFormat::CMYK32 => {
            let src = pixels.get(..n.checked_mul(4)?)?;
            let mut rgba = vec![255u8; n.checked_mul(4)?];
            for (d, p) in rgba.chunks_exact_mut(4).zip(src.chunks_exact(4)) {
                let k = u32::from(p[3]);
                let f = |c: u8| ((u32::from(c) * k + 127) / 255) as u8;
                d[0] = f(p[0]);
                d[1] = f(p[1]);
                d[2] = f(p[2]);
            }
            rgba
        }
    };
    Pixmap::from_vec(rgba, IntSize::from_wh(w, h)?)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn b64() {
        assert_eq!(base64_decode(b"aGVsbG8="), b"hello");
        assert_eq!(base64_decode(b"aGVs\nbG8"), b"hello");
        assert_eq!(base64_decode(b"!!!"), b"");
    }

    /// Opaque pixels skip the premultiply: it would not change them.
    #[test]
    fn premultiply_by_255_is_identity() {
        for c in 0..=255u8 {
            assert_eq!(premultiply_u8(c, 255), c);
        }
    }

    /// The decoder one character at a time: the oracle for the grouped one.
    fn base64_decode_plain(text: &[u8]) -> Vec<u8> {
        let mut out = Vec::new();
        let mut acc: u32 = 0;
        let mut bits = 0u32;
        for &c in text {
            let v = match c {
                b'A'..=b'Z' => c - b'A',
                b'a'..=b'z' => c - b'a' + 26,
                b'0'..=b'9' => c - b'0' + 52,
                b'+' | b'-' => 62,
                b'/' | b'_' => 63,
                b'=' => break,
                b' ' | b'\t' | b'\r' | b'\n' | b'\x0c' => continue,
                _ => break,
            };
            acc = (acc << 6) | u32::from(v);
            bits += 6;
            if bits >= 8 {
                bits -= 8;
                out.push((acc >> bits) as u8);
                acc &= (1 << bits) - 1;
            }
        }
        out
    }

    #[test]
    fn b64_groups_match_plain() {
        for c in 0..=255u8 {
            for prefix in [&b""[..], b"A", b"AB", b"ABC", b"ABCD", b"ABCDE"] {
                let mut t = prefix.to_vec();
                t.push(c);
                t.extend_from_slice(b"QUJDREVGRw==");
                assert_eq!(base64_decode(&t), base64_decode_plain(&t), "{t:?}");
            }
        }
        let alphabet = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/-_";
        let mut x = 0x9e37_79b9_7f4a_7c15u64;
        let mut next = move || {
            x ^= x << 13;
            x ^= x >> 7;
            x ^= x << 17;
            x
        };
        for _ in 0..20_000 {
            let len = (next() % 70) as usize;
            let mut t = Vec::with_capacity(len);
            for _ in 0..len {
                let r = next();
                t.push(match r % 40 {
                    0 => b"\n \t\r\x0c"[(r >> 8) as usize % 5],
                    1 if r & 0x700 == 0 => b'=',
                    2 if r & 0xf00 == 0 => (r >> 16) as u8,
                    _ => alphabet[(r >> 8) as usize % alphabet.len()],
                });
            }
            assert_eq!(base64_decode(&t), base64_decode_plain(&t), "{t:?}");
        }
    }
}
