//! Shared test helpers: decode with both decoders, byte-order conversion.
#![allow(dead_code, clippy::pedantic)]

use std::io::Cursor;

/// One decoded page: colour layout description plus native-endian bytes.
#[derive(Debug, Clone, PartialEq)]
pub struct Page {
    pub width: u32,
    pub height: u32,
    pub bytes: Vec<u8>,
}

/// Decode every page with rusty_tiff's interpreted `read_image`.
pub fn ours(file: &[u8]) -> Result<Vec<Page>, String> {
    ours_mode(file, false)
}

/// Decode every page with `read_image_raw` (no photometric interpretation).
pub fn ours_raw(file: &[u8]) -> Result<Vec<Page>, String> {
    ours_mode(file, true)
}

fn ours_mode(file: &[u8], raw: bool) -> Result<Vec<Page>, String> {
    let mut d = rusty_tiff::Decoder::new(Cursor::new(file))
        .map_err(|e| e.to_string())?
        .with_limits(rusty_tiff::Limits::unlimited());
    let mut pages = Vec::new();
    loop {
        let (width, height) = d.dimensions().map_err(|e| e.to_string())?;
        let img = if raw {
            d.read_image_raw()
        } else {
            d.read_image()
        }
        .map_err(|e| e.to_string())?;
        pages.push(Page {
            width,
            height,
            bytes: img.to_ne_bytes(),
        });
        if !d.more_images() {
            break;
        }
        d.next_image().map_err(|e| e.to_string())?;
    }
    Ok(pages)
}

/// Interleave planar planes (each `plane_len` bytes) into chunky order.
fn interleave(buf: &[u8], planes: usize, w: usize, h: usize, bits: usize) -> Vec<u8> {
    if bits >= 8 {
        let sb = bits / 8;
        let plane_len = w * h * sb;
        let mut out = vec![0u8; plane_len * planes];
        for p in 0..planes {
            for i in 0..w * h {
                out[(i * planes + p) * sb..][..sb]
                    .copy_from_slice(&buf[p * plane_len + i * sb..][..sb]);
            }
        }
        out
    } else {
        let in_row = (w * bits).div_ceil(8);
        let out_row = (w * planes * bits).div_ceil(8);
        let plane_len = in_row * h;
        let mut out = vec![0u8; out_row * h];
        for p in 0..planes {
            for y in 0..h {
                for x in 0..w {
                    for k in 0..bits {
                        let sb = x * bits + k;
                        let bit = (buf[p * plane_len + y * in_row + sb / 8] >> (7 - sb % 8)) & 1;
                        let db = (x * planes + p) * bits + k;
                        out[y * out_row + db / 8] |= bit << (7 - db % 8);
                    }
                }
            }
        }
        out
    }
}

/// Decode every page with image-tiff 0.11 (the oracle), planes interleaved.
/// An image-tiff panic (it asserts on some valid layouts) is reported as an
/// `Err` starting with "PANIC".
pub fn theirs(file: &[u8]) -> Result<Vec<Page>, String> {
    let prev = std::panic::take_hook();
    std::panic::set_hook(Box::new(|_| {}));
    let r = std::panic::catch_unwind(|| theirs_inner(file));
    std::panic::set_hook(prev);
    r.unwrap_or_else(|_| Err("PANIC in image-tiff".into()))
}

fn theirs_inner(file: &[u8]) -> Result<Vec<Page>, String> {
    use tiff::decoder::{Decoder, DecodingResult, Limits};
    let mut d = Decoder::new(Cursor::new(file))
        .map_err(|e| e.to_string())?
        .with_limits(Limits::unlimited());
    let mut pages = Vec::new();
    loop {
        let (width, height) = d.dimensions().map_err(|e| e.to_string())?;
        let bits = d.colortype().map_err(|e| e.to_string())?.bit_depth() as usize;
        let mut r = DecodingResult::U8(vec![]);
        let layout = d.read_image_to_buffer(&mut r).map_err(|e| e.to_string())?;
        let mut bytes = r.as_buffer(0).as_bytes().to_vec();
        bytes.truncate(layout.complete_len);
        if layout.planes > 1 {
            bytes = interleave(&bytes, layout.planes, width as usize, height as usize, bits);
        }
        pages.push(Page {
            width,
            height,
            bytes,
        });
        if !d.more_images() {
            break;
        }
        d.next_image().map_err(|e| e.to_string())?;
    }
    Ok(pages)
}

/// Rewrite a little-endian classic TIFF as big-endian. Pixel data is swapped
/// only where that is a plain byte swap (uncompressed, no float predictor);
/// returns `None` for files where it is not (compressed >= 16-bit integer
/// data without predictor 3).
pub fn to_big_endian(le: &[u8]) -> Option<Vec<u8>> {
    let r16 = |b: &[u8], o: usize| u16::from_le_bytes([b[o], b[o + 1]]);
    let r32 = |b: &[u8], o: usize| u32::from_le_bytes([b[o], b[o + 1], b[o + 2], b[o + 3]]);
    if &le[0..4] != b"II\x2A\x00" {
        return None;
    }
    let mut be = le.to_vec();
    be[0..4].copy_from_slice(b"MM\x00\x2A");
    let mut ifd = r32(le, 4) as usize;
    be[4..8].copy_from_slice(&(ifd as u32).to_be_bytes());
    let mut swapped_regions: Vec<(usize, usize, usize)> = Vec::new(); // (off, len, elem)
    while ifd != 0 {
        let n = r16(le, ifd) as usize;
        be[ifd..ifd + 2].copy_from_slice(&(n as u16).to_be_bytes());
        let mut info = std::collections::HashMap::new();
        for k in 0..n {
            let e = ifd + 2 + k * 12;
            let tag = r16(le, e);
            let ty = r16(le, e + 2);
            let count = r32(le, e + 4) as usize;
            let size = match ty {
                1 | 2 | 6 | 7 => 1,
                3 | 8 => 2,
                4 | 9 | 11 | 13 => 4,
                5 | 10 | 12 => 8,
                _ => return None,
            };
            let elem = match ty {
                5 | 10 => 4,
                _ => size,
            };
            be[e..e + 2].copy_from_slice(&tag.to_be_bytes());
            be[e + 2..e + 4].copy_from_slice(&ty.to_be_bytes());
            be[e + 4..e + 8].copy_from_slice(&(count as u32).to_be_bytes());
            let total = count * size;
            let (off, inline) = if total <= 4 {
                (e + 8, true)
            } else {
                (r32(le, e + 8) as usize, false)
            };
            if !inline {
                be[e + 8..e + 12].copy_from_slice(&(off as u32).to_be_bytes());
            }
            for c in be[off..off + total].chunks_exact_mut(elem) {
                c.reverse();
            }
            let vals: Vec<u64> = (0..count)
                .map(|i| match size {
                    1 => u64::from(le[off + i]),
                    2 => u64::from(r16(le, off + 2 * i)),
                    4 => u64::from(r32(le, off + 4 * i)),
                    _ => 0,
                })
                .collect();
            info.insert(tag, vals);
        }
        let next_pos = ifd + 2 + n * 12;
        let next = r32(le, next_pos) as usize;
        be[next_pos..next_pos + 4].copy_from_slice(&(next as u32).to_be_bytes());
        // Pixel data.
        let get = |t: u16| info.get(&t).cloned().unwrap_or_default();
        let bits = get(258).first().copied().unwrap_or(1) as usize;
        let comp = get(259).first().copied().unwrap_or(1);
        let pred = get(317).first().copied().unwrap_or(1);
        if bits >= 16 && pred != 3 {
            if comp != 1 {
                return None;
            }
            let (offs, cnts) = if info.contains_key(&324) {
                (get(324), get(325))
            } else {
                (get(273), get(279))
            };
            for (o, c) in offs.iter().zip(cnts.iter()) {
                swapped_regions.push((*o as usize, *c as usize, bits / 8));
            }
        }
        ifd = next;
    }
    for (o, c, e) in swapped_regions {
        for ch in be[o..o + c].chunks_exact_mut(e) {
            ch.reverse();
        }
    }
    Some(be)
}

/// Tiny deterministic PRNG.
pub struct Rng(pub u64);
impl Rng {
    pub fn next(&mut self) -> u64 {
        self.0 ^= self.0 << 13;
        self.0 ^= self.0 >> 7;
        self.0 ^= self.0 << 17;
        self.0
    }
}
