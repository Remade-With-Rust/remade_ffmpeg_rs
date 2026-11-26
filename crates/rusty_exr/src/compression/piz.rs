//! PIZ: a value-range bitmap and lookup table, a 2-D Haar wavelet per
//! channel, then the 16-bit Huffman coder. Lossless.

use super::{huffman, BlockGeom};
use crate::error::{Error, Result};

const BITMAP_SIZE: usize = 8192;

// ---------------------------------------------------------------------------
// Wavelet (OpenEXR `ImfWav`).

#[inline]
fn wenc14(a: u16, b: u16) -> (u16, u16) {
    let (a, b) = (a as i16 as i32, b as i16 as i32);
    let m = (a + b) >> 1;
    let d = a - b;
    (m as u16, d as u16)
}

#[inline]
fn wdec14(l: u16, h: u16) -> (u16, u16) {
    let (l, h) = (l as i16 as i32, h as i16 as i32);
    let ai = l + (h & 1) + (h >> 1);
    let a = ai as i16;
    let b = (ai - h) as i16;
    (a as u16, b as u16)
}

const A_OFFSET: i32 = 1 << 15;
const MOD_MASK: i32 = (1 << 16) - 1;

#[inline]
fn wenc16(a: u16, b: u16) -> (u16, u16) {
    let ao = (i32::from(a) + A_OFFSET) & MOD_MASK;
    let b = i32::from(b);
    let mut m = (ao + b) >> 1;
    let d = ao - b;
    if d < 0 {
        m = (m + A_OFFSET) & MOD_MASK;
    }
    (m as u16, (d & MOD_MASK) as u16)
}

#[inline]
fn wdec16(l: u16, h: u16) -> (u16, u16) {
    let (m, d) = (i32::from(l), i32::from(h));
    let bb = (m - (d >> 1)) & MOD_MASK;
    let aa = (d + bb - A_OFFSET) & MOD_MASK;
    (aa as u16, bb as u16)
}

/// Forward 2-D wavelet over an `nx` x `ny` grid at element stride `ox`
/// and row stride `oy`.
fn wav_encode(buf: &mut [u16], nx: usize, ox: usize, ny: usize, oy: usize, max: u16) {
    if max < (1 << 14) {
        wav_encode_impl::<true>(buf, nx, ox, ny, oy);
    } else {
        wav_encode_impl::<false>(buf, nx, ox, ny, oy);
    }
}

fn wav_encode_impl<const W14: bool>(buf: &mut [u16], nx: usize, ox: usize, ny: usize, oy: usize) {
    let enc = if W14 { wenc14 } else { wenc16 };
    let n = nx.min(ny);
    let mut p = 1;
    let mut p2 = 2;
    while p2 <= n {
        let oy1 = oy * p;
        let oy2 = oy * p2;
        let ox1 = ox * p;
        let ox2 = ox * p2;
        let ey = oy * (ny - p2);
        let mut py = 0;
        while py <= ey {
            let ex = py + ox * (nx - p2);
            let mut px = py;
            while px <= ex {
                let p01 = px + ox1;
                let p10 = px + oy1;
                let p11 = p10 + ox1;
                let (i00, i01) = enc(buf[px], buf[p01]);
                let (i10, i11) = enc(buf[p10], buf[p11]);
                let (a, b) = enc(i00, i10);
                buf[px] = a;
                buf[p10] = b;
                let (a, b) = enc(i01, i11);
                buf[p01] = a;
                buf[p11] = b;
                px += ox2;
            }
            if nx & p != 0 {
                let p10 = px + oy1;
                let (a, b) = enc(buf[px], buf[p10]);
                buf[px] = a;
                buf[p10] = b;
            }
            py += oy2;
        }
        if ny & p != 0 {
            let ex = py + ox * (nx - p2);
            let mut px = py;
            while px <= ex {
                let p01 = px + ox1;
                let (a, b) = enc(buf[px], buf[p01]);
                buf[px] = a;
                buf[p01] = b;
                px += ox2;
            }
        }
        p = p2;
        p2 <<= 1;
    }
}

/// Inverse 2-D wavelet.
fn wav_decode(buf: &mut [u16], nx: usize, ox: usize, ny: usize, oy: usize, max: u16) {
    if max < (1 << 14) {
        wav_decode_impl::<true>(buf, nx, ox, ny, oy);
    } else {
        wav_decode_impl::<false>(buf, nx, ox, ny, oy);
    }
}

fn wav_decode_impl<const W14: bool>(buf: &mut [u16], nx: usize, ox: usize, ny: usize, oy: usize) {
    let dec = if W14 { wdec14 } else { wdec16 };
    let n = nx.min(ny);
    let mut p = 1;
    while p <= n {
        p <<= 1;
    }
    p >>= 1;
    let mut p2 = p;
    p >>= 1;
    while p >= 1 {
        let oy1 = oy * p;
        let oy2 = oy * p2;
        let ox1 = ox * p;
        let ox2 = ox * p2;
        let ey = oy * (ny - p2);
        let mut py = 0;
        while py <= ey {
            let ex = py + ox * (nx - p2);
            let mut px = py;
            while px <= ex {
                let p01 = px + ox1;
                let p10 = px + oy1;
                let p11 = p10 + ox1;
                let (i00, i10) = dec(buf[px], buf[p10]);
                let (i01, i11) = dec(buf[p01], buf[p11]);
                let (a, b) = dec(i00, i01);
                buf[px] = a;
                buf[p01] = b;
                let (a, b) = dec(i10, i11);
                buf[p10] = a;
                buf[p11] = b;
                px += ox2;
            }
            if nx & p != 0 {
                let p10 = px + oy1;
                let (a, b) = dec(buf[px], buf[p10]);
                buf[px] = a;
                buf[p10] = b;
            }
            py += oy2;
        }
        if ny & p != 0 {
            let ex = py + ox * (nx - p2);
            let mut px = py;
            while px <= ex {
                let p01 = px + ox1;
                let (a, b) = dec(buf[px], buf[p01]);
                buf[px] = a;
                buf[p01] = b;
                px += ox2;
            }
        }
        p2 = p;
        p >>= 1;
    }
}

// ---------------------------------------------------------------------------
// PIZ.

fn planar_u16(geom: &BlockGeom<'_>, raw: &[u8]) -> Vec<u16> {
    let planar = geom.raw_to_planar(raw);
    planar
        .chunks_exact(2)
        .map(|b| u16::from_le_bytes([b[0], b[1]]))
        .collect()
}

pub(crate) fn decompress(geom: &BlockGeom<'_>, data: &[u8]) -> Result<Vec<u8>> {
    let mut c = crate::header::Cursor::new(data);
    let bmin = {
        let b = c.bytes(2)?;
        u16::from_le_bytes([b[0], b[1]]) as usize
    };
    let bmax = {
        let b = c.bytes(2)?;
        u16::from_le_bytes([b[0], b[1]]) as usize
    };
    if bmin >= BITMAP_SIZE || bmax >= BITMAP_SIZE {
        return Err(Error::invalid("PIZ bitmap range"));
    }
    let mut bitmap = [0u8; BITMAP_SIZE];
    if bmin <= bmax {
        bitmap[bmin..=bmax].copy_from_slice(c.bytes(bmax - bmin + 1)?);
    }
    // Reverse lookup table: dense index -> value.
    let mut lut = vec![0u16; 1 << 16];
    let mut k = 0usize;
    for v in 0..(1usize << 16) {
        if v == 0 || bitmap[v >> 3] & (1 << (v & 7)) != 0 {
            lut[k] = v as u16;
            k += 1;
        }
    }
    let max_value = (k - 1) as u16;
    let len = c.i32()?;
    if len < 0 || len as usize > c.remaining() {
        return Err(Error::invalid("PIZ Huffman length"));
    }
    let n16 = geom.raw_size / 2;
    let mut tmp = huffman::decompress(c.bytes(len as usize)?, n16)?;
    let mut start = 0;
    for ch in &geom.chans {
        let spp = ch.ty.bytes() / 2;
        let count = ch.width * ch.height * spp;
        if count > 0 {
            let s = &mut tmp[start..start + count];
            for off in 0..spp {
                wav_decode(
                    &mut s[off..],
                    ch.width,
                    spp,
                    ch.height,
                    ch.width * spp,
                    max_value,
                );
            }
        }
        start += count;
    }
    for v in &mut tmp {
        *v = lut[*v as usize];
    }
    let mut planar = vec![0u8; geom.raw_size];
    for (d, v) in planar.chunks_exact_mut(2).zip(&tmp) {
        d.copy_from_slice(&v.to_le_bytes());
    }
    Ok(geom.planar_to_raw(&planar))
}

pub(crate) fn compress(geom: &BlockGeom<'_>, raw: &[u8]) -> Vec<u8> {
    if raw.is_empty() {
        return Vec::new();
    }
    let mut tmp = planar_u16(geom, raw);
    let mut bitmap = vec![0u8; BITMAP_SIZE];
    for &v in &tmp {
        bitmap[v as usize >> 3] |= 1 << (v & 7);
    }
    bitmap[0] &= !1;
    // exr writes (0, 0) and one bitmap byte when the bitmap is empty.
    let (bmin, bmax) = match bitmap.iter().position(|&b| b != 0) {
        Some(lo) => (lo, bitmap.iter().rposition(|&b| b != 0).unwrap_or(lo)),
        None => (0, 0),
    };
    let mut fwd = vec![0u16; 1 << 16];
    let mut k = 0u16;
    for v in 0..(1usize << 16) {
        if v == 0 || bitmap[v >> 3] & (1 << (v & 7)) != 0 {
            fwd[v] = k;
            k = k.wrapping_add(1);
        }
    }
    let max_value = k.wrapping_sub(1);
    for v in &mut tmp {
        *v = fwd[*v as usize];
    }
    let mut out = Vec::with_capacity(raw.len() / 2);
    out.extend_from_slice(&(bmin as u16).to_le_bytes());
    out.extend_from_slice(&(bmax as u16).to_le_bytes());
    out.extend_from_slice(&bitmap[bmin..=bmax]);
    let mut start = 0;
    for ch in &geom.chans {
        let spp = ch.ty.bytes() / 2;
        let count = ch.width * ch.height * spp;
        if count > 0 {
            let s = &mut tmp[start..start + count];
            for off in 0..spp {
                wav_encode(
                    &mut s[off..],
                    ch.width,
                    spp,
                    ch.height,
                    ch.width * spp,
                    max_value,
                );
            }
        }
        start += count;
    }
    let huf = huffman::compress(&tmp).expect("bounded symbol counts");
    out.extend_from_slice(&(huf.len() as i32).to_le_bytes());
    out.extend_from_slice(&huf);
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn wavelet_round_trip() {
        let mut seed = 99u32;
        for (nx, ny) in [(1, 1), (3, 5), (8, 8), (17, 4), (31, 33), (64, 3)] {
            for max in [100u16, 60000] {
                let orig: Vec<u16> = (0..nx * ny)
                    .map(|_| {
                        seed = seed.wrapping_mul(1_103_515_245).wrapping_add(12345);
                        ((seed >> 8) % (u32::from(max) + 1)) as u16
                    })
                    .collect();
                let mut b = orig.clone();
                wav_encode(&mut b, nx, 1, ny, nx, max);
                wav_decode(&mut b, nx, 1, ny, nx, max);
                assert_eq!(b, orig, "{nx}x{ny} max {max}");
            }
        }
    }
}
