//! JPEG-in-TIFF (compression 7, TIFF Technical Note 2) through `rusty_jpeg`.
//!
//! The shared `JPEGTables` stream (SOI, DQT/DHT, EOI) is spliced in front of
//! each abbreviated strip/tile stream (SOI dropped), giving an ordinary JPEG
//! interchange stream: table segments may precede the frame header.

use crate::error::{TiffError, TiffResult};

/// Decoded chunk: width, height, components, interleaved 8-bit samples.
pub(crate) struct JpegChunk {
    pub(crate) width: u32,
    pub(crate) height: u32,
    pub(crate) components: u16,
    pub(crate) data: Vec<u8>,
}

/// Find the frame header and return (precision, height, width, components).
fn scan_sof(stream: &[u8]) -> Option<(u8, u16, u16, u8)> {
    let mut i = 2usize; // after SOI
    while i + 4 <= stream.len() {
        if stream[i] != 0xFF {
            return None;
        }
        let m = stream[i + 1];
        if m == 0xFF {
            i += 1;
            continue;
        }
        let len = usize::from(u16::from_be_bytes([stream[i + 2], stream[i + 3]]));
        if matches!(m, 0xC0..=0xCF) && !matches!(m, 0xC4 | 0xC8 | 0xCC) {
            let seg = stream.get(i + 4..i + 2 + len)?;
            if seg.len() < 6 {
                return None;
            }
            return Some((
                seg[0],
                u16::from_be_bytes([seg[1], seg[2]]),
                u16::from_be_bytes([seg[3], seg[4]]),
                seg[5],
            ));
        }
        if m == 0xDA || m == 0xD9 {
            return None;
        }
        i += 2 + len;
    }
    None
}

pub(crate) fn decode(
    tables: Option<&[u8]>,
    data: &[u8],
    ycbcr_to_rgb: bool,
    max_bytes: usize,
) -> TiffResult<JpegChunk> {
    let mut stream = Vec::with_capacity(data.len() + tables.map_or(0, <[u8]>::len));
    match tables {
        Some(t) if t.len() >= 4 && t[0] == 0xFF && t[1] == 0xD8 => {
            let end = if t.ends_with(&[0xFF, 0xD9]) {
                t.len() - 2
            } else {
                t.len()
            };
            stream.extend_from_slice(&t[..end]);
            if data.starts_with(&[0xFF, 0xD8]) {
                stream.extend_from_slice(&data[2..]);
            } else {
                stream.extend_from_slice(data);
            }
        }
        _ => stream.extend_from_slice(data),
    }
    if !stream.starts_with(&[0xFF, 0xD8]) {
        return Err(TiffError::format("JPEG chunk does not start with SOI"));
    }
    // Validate the frame header before handing the stream to the JPEG decoder.
    let (precision, h, w, nc) =
        scan_sof(&stream).ok_or_else(|| TiffError::format("JPEG chunk without a frame header"))?;
    if precision != 8 {
        return Err(TiffError::unsupported(format!(
            "{precision}-bit JPEG-in-TIFF"
        )));
    }
    if w == 0 || h == 0 || nc == 0 {
        return Err(TiffError::format("JPEG frame with unusable geometry"));
    }
    if !matches!(nc, 1 | 3 | 4) {
        return Err(TiffError::unsupported(format!(
            "{nc}-component JPEG-in-TIFF (rusty_jpeg decodes 1, 3 or 4)"
        )));
    }
    if usize::from(w) * usize::from(h) * usize::from(nc) > max_bytes {
        return Err(TiffError::limits("JPEG chunk exceeds the decoding limit"));
    }
    let mut d = rusty_jpeg::Decoder::new(stream.as_slice());
    d.set_max_decoding_buffer_size(max_bytes);
    d.set_single_threaded(true);
    match nc {
        3 if ycbcr_to_rgb => d.set_color_transform(rusty_jpeg::ColorTransform::YCbCr),
        3 | 4 => d.set_color_transform(rusty_jpeg::ColorTransform::None),
        _ => {}
    }
    let pixels = d
        .decode()
        .map_err(|e| TiffError::format(format!("JPEG: {e}")))?;
    let info = d
        .info()
        .ok_or_else(|| TiffError::format("JPEG: no frame decoded"))?;
    let components = info.pixel_format.pixel_bytes() as u16;
    let (width, height) = (u32::from(info.width), u32::from(info.height));
    if pixels.len() < width as usize * height as usize * usize::from(components) {
        return Err(TiffError::format("JPEG: short pixel buffer"));
    }
    Ok(JpegChunk {
        width,
        height,
        components,
        data: pixels,
    })
}
