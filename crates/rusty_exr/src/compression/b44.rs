//! B44 / B44A: fixed-rate lossy compression of HALF channels in 4x4 blocks
//! (14 bytes per block; B44A stores flat blocks in 3). Other sample types
//! are stored raw.
//!
//! The encoder is OpenEXR's `pack` (and the `exr` crate's port of it):
//! output is byte-identical to both.

use super::BlockGeom;
use crate::error::{Error, Result};
use crate::header::SampleType;

#[inline]
fn finish(s: &mut [u16; 16]) {
    for v in s.iter_mut() {
        if *v & 0x8000 != 0 {
            *v &= 0x7fff;
        } else {
            *v = !*v;
        }
    }
}

fn unpack14(b: &[u8], s: &mut [u16; 16]) {
    let b = |i: usize| u32::from(b[i]);
    s[0] = ((b(0) << 8) | b(1)) as u16;
    let shift = b(2) >> 2;
    let bias = 0x20u32 << shift;
    let add =
        |prev: u16, v: u32| (u32::from(prev).wrapping_add(v << shift).wrapping_sub(bias)) as u16;
    s[4] = add(s[0], ((b(2) << 4) | (b(3) >> 4)) & 0x3f);
    s[8] = add(s[4], ((b(3) << 2) | (b(4) >> 6)) & 0x3f);
    s[12] = add(s[8], b(4) & 0x3f);
    s[1] = add(s[0], b(5) >> 2);
    s[5] = add(s[4], ((b(5) << 4) | (b(6) >> 4)) & 0x3f);
    s[9] = add(s[8], ((b(6) << 2) | (b(7) >> 6)) & 0x3f);
    s[13] = add(s[12], b(7) & 0x3f);
    s[2] = add(s[1], b(8) >> 2);
    s[6] = add(s[5], ((b(8) << 4) | (b(9) >> 4)) & 0x3f);
    s[10] = add(s[9], ((b(9) << 2) | (b(10) >> 6)) & 0x3f);
    s[14] = add(s[13], b(10) & 0x3f);
    s[3] = add(s[2], b(11) >> 2);
    s[7] = add(s[6], ((b(11) << 4) | (b(12) >> 4)) & 0x3f);
    s[11] = add(s[10], ((b(12) << 2) | (b(13) >> 6)) & 0x3f);
    s[15] = add(s[14], b(13) & 0x3f);
    finish(s);
}

fn unpack3(b: &[u8], s: &mut [u16; 16]) {
    *s = [((u16::from(b[0])) << 8) | u16::from(b[1]); 16];
    finish(s);
}

/// Decode a B44 chunk into `planar` (channel-planar bytes, exactly
/// `raw_size` long; every byte is written, so a reused buffer needs no
/// clearing).
pub(crate) fn decompress(geom: &BlockGeom<'_>, data: &[u8], planar: &mut [u8]) -> Result<()> {
    assert_eq!(planar.len(), geom.raw_size);
    let log = &crate::luts::get().b44_log;
    let mut ip = 0usize;
    let mut op = 0usize;
    for ch in &geom.chans {
        let n = ch.bytes();
        if n == 0 {
            continue;
        }
        if ch.ty != SampleType::F16 {
            if ip + n > data.len() {
                return Err(Error::invalid("B44 chunk truncated"));
            }
            planar[op..op + n].copy_from_slice(&data[ip..ip + n]);
            ip += n;
            op += n;
            continue;
        }
        let (nx, ny) = (ch.width, ch.height);
        let out = &mut planar[op..op + n];
        let lut = ch.p_linear.then_some(&log[..]);
        // A SIMD prefix of the blocks (raster block order), then the rest.
        let (done, at) = crate::simd::b44_channel(crate::simd::isa(), data, ip, nx, ny, lut, out);
        ip = at;
        let nbx = nx.div_ceil(4);
        for b in done..nbx * ny.div_ceil(4) {
            ip = block_scalar(data, ip, (b % nbx) * 4, (b / nbx) * 4, nx, ny, lut, out)?;
        }
        op += n;
    }
    Ok(())
}

/// Decode the block at `ip` into the 4x4 cell at (`x`, `y`) of a HALF
/// plane (`out`, little-endian, `nx` x `ny`); returns the next block's
/// offset. The oracle of `simd::b44_channel`.
pub(crate) fn block_scalar(
    data: &[u8],
    ip: usize,
    x: usize,
    y: usize,
    nx: usize,
    ny: usize,
    lut: Option<&[u16]>,
    out: &mut [u8],
) -> Result<usize> {
    let mut s = [0u16; 16];
    if ip + 3 > data.len() {
        return Err(Error::invalid("B44 chunk truncated"));
    }
    let next = if data[ip + 2] >= (13 << 2) {
        unpack3(&data[ip..ip + 3], &mut s);
        ip + 3
    } else {
        if ip + 14 > data.len() {
            return Err(Error::invalid("B44 chunk truncated"));
        }
        unpack14(&data[ip..ip + 14], &mut s);
        ip + 14
    };
    if let Some(log) = lut {
        for v in &mut s {
            *v = log[*v as usize];
        }
    }
    let cols = (nx - x).min(4);
    let rows = (ny - y).min(4);
    for r in 0..rows {
        let o = ((y + r) * nx + x) * 2;
        for k in 0..cols {
            out[o + 2 * k..o + 2 * k + 2].copy_from_slice(&s[r * 4 + k].to_le_bytes());
        }
    }
    Ok(next)
}

/// OpenEXR's `shiftAndRound`: `x / 2^shift`, rounded to nearest, ties to even.
#[inline]
fn shift_and_round(x: i32, shift: i32) -> i32 {
    let x = x << 1;
    let a = (1 << shift) - 1;
    let shift = shift + 1;
    let b = (x >> shift) & 1;
    (x + a + b) >> shift
}

const BIAS: i32 = 0x20;

/// The search half of `pack`: the block's sortable values `t`, their
/// maximum, the smallest shift at which every delta fits in 6 bits, and the
/// rounded distances `d` from the maximum at that shift.
pub(crate) struct Search {
    pub t: [u16; 16],
    pub t_max: i32,
    pub shift: i32,
    pub d: [i32; 16],
}

/// OpenEXR's `pack` search, scalar (the oracle of `simd::b44_search`).
pub(crate) fn search_scalar(s: &[u16; 16]) -> Search {
    let mut t = [0u16; 16];
    for (t, &v) in t.iter_mut().zip(s) {
        *t = if v & 0x7c00 == 0x7c00 {
            0x8000
        } else if v & 0x8000 != 0 {
            !v
        } else {
            v | 0x8000
        };
    }
    let t_max = i32::from(*t.iter().max().expect("16 values"));
    let mut shift = -1;
    let mut d = [0i32; 16];
    loop {
        shift += 1;
        for (d, &t) in d.iter_mut().zip(&t) {
            *d = shift_and_round(t_max - i32::from(t), shift);
        }
        let r = deltas(&d);
        let (lo, hi) = r
            .iter()
            .fold((r[0], r[0]), |(lo, hi), &v| (lo.min(v), hi.max(v)));
        if lo >= 0 && hi <= 0x3f {
            return Search { t, t_max, shift, d };
        }
    }
}

/// The 15 biased deltas B44 stores, from the rounded distances.
fn deltas(d: &[i32; 16]) -> [i32; 15] {
    [
        d[0] - d[4] + BIAS,
        d[4] - d[8] + BIAS,
        d[8] - d[12] + BIAS,
        d[0] - d[1] + BIAS,
        d[4] - d[5] + BIAS,
        d[8] - d[9] + BIAS,
        d[12] - d[13] + BIAS,
        d[1] - d[2] + BIAS,
        d[5] - d[6] + BIAS,
        d[9] - d[10] + BIAS,
        d[13] - d[14] + BIAS,
        d[2] - d[3] + BIAS,
        d[6] - d[7] + BIAS,
        d[10] - d[11] + BIAS,
        d[14] - d[15] + BIAS,
    ]
}

/// Pack one 4x4 block of halves; returns the bytes written (14, or 3 for
/// a flat block when `flat` is set).
fn pack(
    isa: crate::simd::Isa,
    s: &[u16; 16],
    b: &mut [u8; 14],
    flat: bool,
    exact_max: bool,
) -> usize {
    let Search {
        mut t,
        t_max,
        shift,
        d,
    } = crate::simd::b44_search(isa, s).unwrap_or_else(|| search_scalar(s));
    let r = deltas(&d);
    if flat && r.iter().all(|&v| v == BIAS) {
        b[0] = (t[0] >> 8) as u8;
        b[1] = t[0] as u8;
        b[2] = 0xfc;
        return 3;
    }
    if exact_max {
        t[0] = (t_max - (d[0] << shift)) as u16;
    }
    b[0] = (t[0] >> 8) as u8;
    b[1] = t[0] as u8;
    let r = r.map(|v| v as u32);
    let shift = shift as u32;
    b[2] = ((shift << 2) | (r[0] >> 4)) as u8;
    b[3] = ((r[0] << 4) | (r[1] >> 2)) as u8;
    b[4] = ((r[1] << 6) | r[2]) as u8;
    b[5] = ((r[3] << 2) | (r[4] >> 4)) as u8;
    b[6] = ((r[4] << 4) | (r[5] >> 2)) as u8;
    b[7] = ((r[5] << 6) | r[6]) as u8;
    b[8] = ((r[7] << 2) | (r[8] >> 4)) as u8;
    b[9] = ((r[8] << 4) | (r[9] >> 2)) as u8;
    b[10] = ((r[9] << 6) | r[10]) as u8;
    b[11] = ((r[11] << 2) | (r[12] >> 4)) as u8;
    b[12] = ((r[12] << 4) | (r[13] >> 2)) as u8;
    b[13] = ((r[13] << 6) | r[14]) as u8;
    14
}

/// Compress one chunk (raw layout in, B44 stream out). `flat` selects B44A.
pub(crate) fn compress(
    geom: &BlockGeom<'_>,
    raw: &[u8],
    flat: bool,
    work: &mut super::Work,
) -> Vec<u8> {
    let exp = &crate::luts::get().b44_exp;
    let isa = crate::simd::isa();
    let planar = super::grow(&mut work.planar, geom.raw_size);
    geom.raw_to_planar_into(raw, planar);
    let planar = &*planar;
    let mut out = Vec::with_capacity(raw.len());
    let mut ip = 0usize;
    let mut block = [0u8; 14];
    for ch in &geom.chans {
        let n = ch.bytes();
        let src = &planar[ip..ip + n];
        ip += n;
        if n == 0 {
            continue;
        }
        if ch.ty != SampleType::F16 {
            out.extend_from_slice(src);
            continue;
        }
        let (nx, ny) = (ch.width, ch.height);
        let at = |x: usize, y: usize| {
            let o = (y * nx + x) * 2;
            u16::from_le_bytes([src[o], src[o + 1]])
        };
        let mut s = [0u16; 16];
        for y in (0..ny).step_by(4) {
            // Byte offsets of the block row's four lines (edge blocks repeat
            // the last line), computed once per block row.
            let lines: [usize; 4] = std::array::from_fn(|r| (y + r).min(ny - 1) * nx * 2);
            for x in (0..nx).step_by(4) {
                if x + 4 <= nx {
                    for (r, &l) in lines.iter().enumerate() {
                        let row = &src[l + 2 * x..l + 2 * x + 8];
                        for k in 0..4 {
                            s[r * 4 + k] = u16::from_le_bytes([row[2 * k], row[2 * k + 1]]);
                        }
                    }
                } else {
                    // Edge blocks repeat the last column.
                    for r in 0..4 {
                        let yy = (y + r).min(ny - 1);
                        for k in 0..4 {
                            s[r * 4 + k] = at((x + k).min(nx - 1), yy);
                        }
                    }
                }
                if ch.p_linear {
                    for v in &mut s {
                        *v = exp[*v as usize];
                    }
                }
                let len = pack(isa, &s, &mut block, flat, !ch.p_linear);
                out.extend_from_slice(&block[..len]);
            }
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The search twin against the scalar search: random blocks over the
    /// whole half range, smooth blocks (small shifts), flat blocks, specials.
    #[test]
    fn search_twin_matches_scalar() {
        let mut seed = 0x5eed_b44au64;
        let mut next = || {
            seed ^= seed << 13;
            seed ^= seed >> 7;
            seed ^= seed << 17;
            seed
        };
        let rounds = if cfg!(debug_assertions) {
            20_000
        } else {
            500_000
        };
        for k in 0..rounds {
            let base = next() as u16;
            let s: [u16; 16] = std::array::from_fn(|_| match k % 5 {
                0 => next() as u16,
                1 => base.wrapping_add((next() % 9) as u16),
                2 => base,
                3 => [0x7c00, 0xfc00, 0x7e01, 0, 0x8000, 0x3c00][(next() % 6) as usize],
                _ => base.wrapping_add((next() % 300) as u16),
            });
            let want = search_scalar(&s);
            for isa in crate::simd::available() {
                if let Some(got) = crate::simd::b44_search(isa, &s) {
                    assert_eq!(
                        (got.t, got.t_max, got.shift, got.d),
                        (want.t, want.t_max, want.shift, want.d),
                        "{isa:?} {s:04x?}"
                    );
                }
            }
        }
    }

    /// The SIMD block prefix followed by the scalar blocks against the
    /// scalar blocks alone: random 14- and 3-byte blocks (every shift,
    /// every field value), edge cells, the transfer table, short data.
    #[test]
    fn channel_twin_matches_scalar() {
        let mut seed = 0x0b44_0b44u64;
        let mut next = || {
            seed ^= seed << 13;
            seed ^= seed >> 7;
            seed ^= seed << 17;
            seed
        };
        let log = &crate::luts::get().b44_log;
        for &(nx, ny) in &[
            (1usize, 1usize),
            (4, 4),
            (5, 3),
            (8, 8),
            (13, 9),
            (64, 7),
            (33, 33),
        ] {
            for round in 0..40 {
                let nblocks = nx.div_ceil(4) * ny.div_ceil(4);
                let mut data = Vec::new();
                for _ in 0..nblocks {
                    let r = next();
                    if r % 5 == 0 {
                        data.extend_from_slice(&[(r >> 8) as u8, (r >> 16) as u8, 0xfc]);
                    } else {
                        let mut b: Vec<u8> = (0..14).map(|_| next() as u8).collect();
                        // Shift 0..=12 (13 and up mark a 3-byte block).
                        b[2] = ((r % 13) as u8) << 2 | (b[2] & 3);
                        data.extend_from_slice(&b);
                    }
                }
                let cut = if round % 7 == 6 {
                    data.len() - (next() as usize % 20).min(data.len())
                } else {
                    data.len()
                };
                let data = &data[..cut];
                for lut in [None, Some(&log[..])] {
                    let mut want = vec![0u8; nx * ny * 2];
                    let mut ip = 0;
                    let nbx = nx.div_ceil(4);
                    let mut want_err = false;
                    for b in 0..nblocks {
                        let (x, y) = ((b % nbx) * 4, (b / nbx) * 4);
                        let Ok(n) = block_scalar(data, ip, x, y, nx, ny, lut, &mut want) else {
                            want_err = true;
                            break;
                        };
                        ip = n;
                    }
                    for isa in crate::simd::available() {
                        let mut got = vec![0u8; nx * ny * 2];
                        let (done, mut ip2) =
                            crate::simd::b44_channel(isa, data, 0, nx, ny, lut, &mut got);
                        let mut err = false;
                        for b in done..nblocks {
                            let (x, y) = ((b % nbx) * 4, (b / nbx) * 4);
                            let Ok(n) = block_scalar(data, ip2, x, y, nx, ny, lut, &mut got) else {
                                err = true;
                                break;
                            };
                            ip2 = n;
                        }
                        assert_eq!(err, want_err, "{isa:?} {nx}x{ny}");
                        if !err {
                            assert_eq!(ip2, ip, "{isa:?} {nx}x{ny}");
                            assert_eq!(got, want, "{isa:?} {nx}x{ny} lut {}", lut.is_some());
                        }
                    }
                }
            }
        }
    }
}
