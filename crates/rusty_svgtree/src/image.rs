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
pub(crate) fn base64_decode(text: &[u8]) -> Vec<u8> {
    let mut out = Vec::with_capacity(text.len() / 4 * 3 + 3);
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
    let mut buf = vec![0; reader.output_buffer_size()];
    let out = reader.next_frame(&mut buf).ok()?;
    if out.bit_depth != rusty_png::BitDepth::Eight {
        return None;
    }
    let size = IntSize::from_wh(out.width, out.height)?;
    let n = (out.width as usize).checked_mul(out.height as usize)?;
    let mut rgba = Vec::with_capacity(n.checked_mul(4)?);
    match out.color_type {
        rusty_png::ColorType::Rgb => {
            for p in buf.chunks_exact(3).take(n) {
                rgba.extend_from_slice(&[p[0], p[1], p[2], 255]);
            }
        }
        rusty_png::ColorType::Rgba => rgba.extend_from_slice(buf.get(..n * 4)?),
        rusty_png::ColorType::Grayscale => {
            for &g in buf.iter().take(n) {
                rgba.extend_from_slice(&[g, g, g, 255]);
            }
        }
        rusty_png::ColorType::GrayscaleAlpha => {
            for p in buf.chunks_exact(2).take(n) {
                rgba.extend_from_slice(&[p[0], p[0], p[0], p[1]]);
            }
        }
        rusty_png::ColorType::Indexed => return None,
    }
    if rgba.len() != n * 4 {
        return None;
    }
    for p in rgba.chunks_exact_mut(4) {
        let a = p[3];
        p[0] = premultiply_u8(p[0], a);
        p[1] = premultiply_u8(p[1], a);
        p[2] = premultiply_u8(p[2], a);
    }
    Pixmap::from_vec(rgba, size)
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
    let mut rgba = Vec::with_capacity(n.checked_mul(4)?);
    match info.pixel_format {
        rusty_jpeg::PixelFormat::L8 => {
            for &g in pixels.iter().take(n) {
                rgba.extend_from_slice(&[g, g, g, 255]);
            }
        }
        rusty_jpeg::PixelFormat::L16 => {
            for p in pixels.chunks_exact(2).take(n) {
                rgba.extend_from_slice(&[p[0], p[0], p[0], 255]);
            }
        }
        rusty_jpeg::PixelFormat::RGB24 => {
            for p in pixels.chunks_exact(3).take(n) {
                rgba.extend_from_slice(&[p[0], p[1], p[2], 255]);
            }
        }
        rusty_jpeg::PixelFormat::CMYK32 => {
            for p in pixels.chunks_exact(4).take(n) {
                let k = u32::from(p[3]);
                let f = |c: u8| ((u32::from(c) * k + 127) / 255) as u8;
                rgba.extend_from_slice(&[f(p[0]), f(p[1]), f(p[2]), 255]);
            }
        }
    }
    if rgba.len() != n * 4 {
        return None;
    }
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
}
