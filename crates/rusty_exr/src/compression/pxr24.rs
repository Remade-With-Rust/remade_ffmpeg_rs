//! PXR24: float samples rounded to 24 bits, per-row deltas split into byte
//! planes, then zlib. Lossless for HALF and UINT.

use super::{sampled, BlockGeom};
use crate::error::{Error, Result};
use crate::header::SampleType;

/// OpenEXR's `floatToFloat24`: round the significand to 15 bits, keeping
/// NaN a NaN and avoiding exponent overflow near `f32::MAX`.
pub(crate) fn f32_to_f24(bits: u32) -> u32 {
    let sign = bits & 0x8000_0000;
    let exp = bits & 0x7f80_0000;
    let man = bits & 0x007f_ffff;
    let r = if exp == 0x7f80_0000 {
        if man != 0 {
            let m = man >> 8;
            (exp >> 8) | m | u32::from(m == 0)
        } else {
            exp >> 8
        }
    } else {
        let r = ((exp | man) + (man & 0x0000_0080)) >> 8;
        if r >= 0x7f_8000 {
            (exp | man) >> 8
        } else {
            r
        }
    };
    (sign >> 8) | r
}

/// Bytes per sample in the packed stream.
fn packed_bytes(t: SampleType) -> usize {
    match t {
        SampleType::F16 => 2,
        SampleType::F32 => 3,
        SampleType::U32 => 4,
    }
}

/// One HALF row: running sum of the big-endian 16-bit deltas in the byte
/// planes `hi`/`lo`, written little-endian (the oracle of
/// `simd::pxr24_row_16`).
pub(crate) fn row_16(hi: &[u8], lo: &[u8], out: &mut [u8]) {
    let mut acc = 0u16;
    for ((d, &h), &l) in out.chunks_exact_mut(2).zip(hi).zip(lo) {
        acc = acc.wrapping_add(u16::from_be_bytes([h, l]));
        d.copy_from_slice(&acc.to_le_bytes());
    }
}

/// One FLOAT (three planes, low byte zero) or UINT (four planes) row: the
/// running sum of the big-endian 32-bit deltas (the oracle of
/// `simd::pxr24_row_32`).
pub(crate) fn row_32(p: [&[u8]; 3], p3: Option<&[u8]>, out: &mut [u8]) {
    let mut acc = 0u32;
    for (x, d) in out.chunks_exact_mut(4).enumerate() {
        let low = p3.map_or(0, |q| q[x]);
        acc = acc.wrapping_add(u32::from_be_bytes([p[0][x], p[1][x], p[2][x], low]));
        d.copy_from_slice(&acc.to_le_bytes());
    }
}

/// Decode a PXR24 chunk into `scratch.a` (raw layout).
pub(crate) fn decompress(
    geom: &BlockGeom<'_>,
    data: &[u8],
    scratch: &mut super::Scratch,
) -> Result<()> {
    let packed_size: usize = geom
        .chans
        .iter()
        .map(|c| c.width * c.height * packed_bytes(c.ty))
        .sum();
    scratch
        .inflate_b(data, packed_size)
        .map_err(|e| Error::invalid(format!("PXR24 chunk: {e}")))?;
    let packed = &scratch.b;
    let out = &mut scratch.a;
    // Every byte is written below, so a reused buffer is not re-zeroed.
    out.resize(geom.raw_size, 0);
    let isa = crate::simd::isa();
    let mut ip = 0;
    let mut op = 0;
    for line in 0..geom.lines {
        let y = geom.y0 + line as i32;
        for c in &geom.chans {
            if !sampled(y, c.y_sampling) {
                continue;
            }
            let w = c.width;
            match c.ty {
                SampleType::F16 => {
                    let (p0, p1) = packed[ip..ip + 2 * w].split_at(w);
                    let dst = &mut out[op..op + 2 * w];
                    if !crate::simd::pxr24_row_16(isa, p0, p1, dst) {
                        row_16(p0, p1, dst);
                    }
                    ip += 2 * w;
                    op += 2 * w;
                }
                SampleType::U32 => {
                    let (p0, rest) = packed[ip..ip + 4 * w].split_at(w);
                    let (p1, rest) = rest.split_at(w);
                    let (p2, p3) = rest.split_at(w);
                    let dst = &mut out[op..op + 4 * w];
                    if !crate::simd::pxr24_row_32(isa, [p0, p1, p2], Some(p3), dst) {
                        row_32([p0, p1, p2], Some(p3), dst);
                    }
                    ip += 4 * w;
                    op += 4 * w;
                }
                SampleType::F32 => {
                    let (p0, rest) = packed[ip..ip + 3 * w].split_at(w);
                    let (p1, p2) = rest.split_at(w);
                    let dst = &mut out[op..op + 4 * w];
                    if !crate::simd::pxr24_row_32(isa, [p0, p1, p2], None, dst) {
                        row_32([p0, p1, p2], None, dst);
                    }
                    ip += 3 * w;
                    op += 4 * w;
                }
            }
        }
    }
    Ok(())
}

/// One row of the encoder: the samples in `src` (little-endian) as
/// big-endian deltas from their left neighbour (FLOAT through
/// [`f32_to_f24`] first), split into byte planes in `dst` (most significant
/// plane first; FLOAT keeps three). The oracle of `simd::pxr24_enc_row`.
pub(crate) fn enc_row(ty: SampleType, src: &[u8], dst: &mut [u8]) {
    let n = packed_bytes(ty);
    let w = dst.len() / n;
    match ty {
        SampleType::F16 => {
            let mut prev = 0u16;
            for x in 0..w {
                let v = u16::from_le_bytes([src[2 * x], src[2 * x + 1]]);
                let d = v.wrapping_sub(prev).to_be_bytes();
                prev = v;
                dst[x] = d[0];
                dst[w + x] = d[1];
            }
        }
        SampleType::U32 | SampleType::F32 => {
            let f24 = ty == SampleType::F32;
            let mut prev = 0u32;
            for x in 0..w {
                let b = &src[4 * x..4 * x + 4];
                let mut v = u32::from_le_bytes([b[0], b[1], b[2], b[3]]);
                if f24 {
                    v = f32_to_f24(v);
                }
                let d = v.wrapping_sub(prev).to_be_bytes();
                prev = v;
                for k in 0..n {
                    dst[k * w + x] = d[4 - n + k];
                }
            }
        }
    }
}

pub(crate) fn compress(geom: &BlockGeom<'_>, raw: &[u8], deflater: crate::Deflate) -> Vec<u8> {
    let packed_size: usize = geom
        .chans
        .iter()
        .map(|c| c.width * c.height * packed_bytes(c.ty))
        .sum();
    let mut packed = vec![0u8; packed_size];
    let isa = crate::simd::isa();
    let mut ip = 0;
    let mut op = 0;
    for line in 0..geom.lines {
        let y = geom.y0 + line as i32;
        for c in &geom.chans {
            if !sampled(y, c.y_sampling) {
                continue;
            }
            let w = c.width;
            let n = packed_bytes(c.ty);
            let src = &raw[ip..ip + c.ty.bytes() * w];
            let dst = &mut packed[op..op + n * w];
            if !crate::simd::pxr24_enc_row(isa, c.ty, src, dst) {
                enc_row(c.ty, src, dst);
            }
            ip += c.ty.bytes() * w;
            op += n * w;
        }
    }
    super::deflate(&packed, deflater)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn f24_twin_matches_scalar() {
        // Every pattern in release builds; a dense stride in debug ones.
        let step: u64 = if cfg!(debug_assertions) { 1021 } else { 1 };
        for isa in crate::simd::available() {
            let mut x = 0u64;
            while u32::try_from(x).is_ok() {
                let v: [u32; 4] = std::array::from_fn(|k| (x as u32).wrapping_add(k as u32));
                let got = crate::simd::f24_4(isa, v);
                for k in 0..4 {
                    assert_eq!(got[k], f32_to_f24(v[k]), "{isa:?} {:08x}", v[k]);
                }
                x += 4 * step;
            }
        }
    }

    #[test]
    fn encoder_row_twins_match_scalar() {
        let mut seed = 0x2468_ace0u32;
        let mut next = || {
            seed ^= seed << 13;
            seed ^= seed >> 17;
            seed ^= seed << 5;
            seed
        };
        for w in [0usize, 1, 3, 7, 8, 9, 15, 16, 17, 31, 64, 100, 257] {
            for ty in [SampleType::F16, SampleType::F32, SampleType::U32] {
                let src: Vec<u8> = (0..ty.bytes() * w).map(|_| next() as u8).collect();
                let n = packed_bytes(ty);
                let mut want = vec![0u8; n * w];
                enc_row(ty, &src, &mut want);
                for isa in crate::simd::available() {
                    let mut got = vec![0u8; n * w];
                    assert!(crate::simd::pxr24_enc_row(isa, ty, &src, &mut got));
                    assert_eq!(got, want, "{isa:?} {ty:?} w {w}");
                }
            }
        }
    }

    #[test]
    fn row_twins_match_scalar() {
        let mut seed = 0x1357_9bdfu32;
        let mut next = || {
            seed ^= seed << 13;
            seed ^= seed >> 17;
            seed ^= seed << 5;
            seed as u8
        };
        for w in [0usize, 1, 3, 7, 8, 9, 15, 16, 17, 31, 64, 100, 257] {
            let planes: Vec<Vec<u8>> = (0..4).map(|_| (0..w).map(|_| next()).collect()).collect();
            let mut want = vec![0u8; 2 * w];
            row_16(&planes[0], &planes[1], &mut want);
            let mut want3 = vec![0u8; 4 * w];
            row_32([&planes[0], &planes[1], &planes[2]], None, &mut want3);
            let mut want4 = vec![0u8; 4 * w];
            row_32(
                [&planes[0], &planes[1], &planes[2]],
                Some(&planes[3]),
                &mut want4,
            );
            for isa in crate::simd::available() {
                let mut got = vec![0u8; 2 * w];
                assert!(crate::simd::pxr24_row_16(
                    isa, &planes[0], &planes[1], &mut got
                ));
                assert_eq!(got, want, "{isa:?} f16 w {w}");
                let mut got = vec![0u8; 4 * w];
                assert!(crate::simd::pxr24_row_32(
                    isa,
                    [&planes[0], &planes[1], &planes[2]],
                    None,
                    &mut got
                ));
                assert_eq!(got, want3, "{isa:?} f32 w {w}");
                let mut got = vec![0u8; 4 * w];
                let p3 = Some(&planes[3][..]);
                assert!(crate::simd::pxr24_row_32(
                    isa,
                    [&planes[0], &planes[1], &planes[2]],
                    p3,
                    &mut got
                ));
                assert_eq!(got, want4, "{isa:?} u32 w {w}");
            }
        }
    }
}
