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
                    let mut acc = 0u16;
                    for ((d, &hi), &lo) in out[op..op + 2 * w].chunks_exact_mut(2).zip(p0).zip(p1) {
                        acc = acc.wrapping_add(u16::from_be_bytes([hi, lo]));
                        d.copy_from_slice(&acc.to_le_bytes());
                    }
                    ip += 2 * w;
                    op += 2 * w;
                }
                SampleType::U32 => {
                    let (p0, rest) = packed[ip..ip + 4 * w].split_at(w);
                    let (p1, rest) = rest.split_at(w);
                    let (p2, p3) = rest.split_at(w);
                    let mut acc = 0u32;
                    for ((((d, &a), &b), &c), &e) in out[op..op + 4 * w]
                        .chunks_exact_mut(4)
                        .zip(p0)
                        .zip(p1)
                        .zip(p2)
                        .zip(p3)
                    {
                        acc = acc.wrapping_add(u32::from_be_bytes([a, b, c, e]));
                        d.copy_from_slice(&acc.to_le_bytes());
                    }
                    ip += 4 * w;
                    op += 4 * w;
                }
                SampleType::F32 => {
                    let (p0, rest) = packed[ip..ip + 3 * w].split_at(w);
                    let (p1, p2) = rest.split_at(w);
                    let mut acc = 0u32;
                    for (((d, &a), &b), &c) in out[op..op + 4 * w]
                        .chunks_exact_mut(4)
                        .zip(p0)
                        .zip(p1)
                        .zip(p2)
                    {
                        acc = acc.wrapping_add(u32::from_be_bytes([a, b, c, 0]));
                        d.copy_from_slice(&acc.to_le_bytes());
                    }
                    ip += 3 * w;
                    op += 4 * w;
                }
            }
        }
    }
    Ok(())
}

pub(crate) fn compress(geom: &BlockGeom<'_>, raw: &[u8], deflater: crate::Deflate) -> Vec<u8> {
    let packed_size: usize = geom
        .chans
        .iter()
        .map(|c| c.width * c.height * packed_bytes(c.ty))
        .sum();
    let mut packed = vec![0u8; packed_size];
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
                    let mut prev = 0u16;
                    for x in 0..w {
                        let v = u16::from_le_bytes([raw[ip + 2 * x], raw[ip + 2 * x + 1]]);
                        let d = v.wrapping_sub(prev).to_be_bytes();
                        prev = v;
                        packed[op + x] = d[0];
                        packed[op + w + x] = d[1];
                    }
                    ip += 2 * w;
                    op += 2 * w;
                }
                SampleType::U32 => {
                    let mut prev = 0u32;
                    for x in 0..w {
                        let b = &raw[ip + 4 * x..ip + 4 * x + 4];
                        let v = u32::from_le_bytes([b[0], b[1], b[2], b[3]]);
                        let d = v.wrapping_sub(prev).to_be_bytes();
                        prev = v;
                        for k in 0..4 {
                            packed[op + k * w + x] = d[k];
                        }
                    }
                    ip += 4 * w;
                    op += 4 * w;
                }
                SampleType::F32 => {
                    let mut prev = 0u32;
                    for x in 0..w {
                        let b = &raw[ip + 4 * x..ip + 4 * x + 4];
                        let v = f32_to_f24(u32::from_le_bytes([b[0], b[1], b[2], b[3]]));
                        let d = v.wrapping_sub(prev).to_be_bytes();
                        prev = v;
                        for k in 0..3 {
                            packed[op + k * w + x] = d[k + 1];
                        }
                    }
                    ip += 4 * w;
                    op += 3 * w;
                }
            }
        }
    }
    super::deflate(&packed, deflater)
}
