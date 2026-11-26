//! B44 / B44A: fixed-rate lossy compression of HALF channels in 4x4 blocks
//! (14 bytes per block; B44A stores flat blocks in 3). Other sample types
//! are stored raw.

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

pub(crate) fn decompress(geom: &BlockGeom<'_>, data: &[u8]) -> Result<Vec<u8>> {
    let log = &crate::luts::get().b44_log;
    let mut planar = vec![0u8; geom.raw_size];
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
        let mut s = [0u16; 16];
        for y in (0..ny).step_by(4) {
            for x in (0..nx).step_by(4) {
                if ip + 3 > data.len() {
                    return Err(Error::invalid("B44 chunk truncated"));
                }
                if data[ip + 2] >= (13 << 2) {
                    unpack3(&data[ip..ip + 3], &mut s);
                    ip += 3;
                } else {
                    if ip + 14 > data.len() {
                        return Err(Error::invalid("B44 chunk truncated"));
                    }
                    unpack14(&data[ip..ip + 14], &mut s);
                    ip += 14;
                }
                if ch.p_linear {
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
            }
        }
        op += n;
    }
    Ok(planar)
}
