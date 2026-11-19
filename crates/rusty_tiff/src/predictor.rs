//! Predictors 2 (horizontal differencing) and 3 (floating-point byte-plane
//! differencing), forward and inverse, plus byte-order helpers.
//!
//! All functions work row by row on byte buffers whose multi-byte samples are
//! in NATIVE byte order, except the float predictor's coded form, which is
//! big-endian byte planes by definition (libtiff `fpAcc`/`fpDiff`).

/// Reverse the byte order of every `n`-byte sample in place.
pub(crate) fn swap_bytes(buf: &mut [u8], n: usize) {
    if n > 1 {
        for c in buf.chunks_exact_mut(n) {
            c.reverse();
        }
    }
}

macro_rules! hpred {
    ($name:ident, $enc:ident, $t:ty, $n:expr) => {
        fn $name(row: &mut [u8], spp: usize) {
            let stride = spp * $n;
            for i in (stride..row.len() - row.len() % $n).step_by($n) {
                let v = <$t>::from_ne_bytes(row[i..i + $n].try_into().unwrap_or([0; $n]));
                let p = <$t>::from_ne_bytes(
                    row[i - stride..i - stride + $n]
                        .try_into()
                        .unwrap_or([0; $n]),
                );
                row[i..i + $n].copy_from_slice(&v.wrapping_add(p).to_ne_bytes());
            }
        }
        fn $enc(row: &mut [u8], spp: usize) {
            let stride = spp * $n;
            let end = row.len() - row.len() % $n;
            let mut i = end;
            while i >= stride + $n {
                i -= $n;
                let v = <$t>::from_ne_bytes(row[i..i + $n].try_into().unwrap_or([0; $n]));
                let p = <$t>::from_ne_bytes(
                    row[i - stride..i - stride + $n]
                        .try_into()
                        .unwrap_or([0; $n]),
                );
                row[i..i + $n].copy_from_slice(&v.wrapping_sub(p).to_ne_bytes());
            }
        }
    };
}

hpred!(undo_h16, do_h16, u16, 2);
hpred!(undo_h32, do_h32, u32, 4);
hpred!(undo_h64, do_h64, u64, 8);

/// Undo horizontal differencing on each `row_bytes` row.
pub(crate) fn undo_horizontal(buf: &mut [u8], row_bytes: usize, spp: usize, bytes: usize) {
    if row_bytes == 0 {
        return;
    }
    for row in buf.chunks_mut(row_bytes) {
        match bytes {
            1 => {
                for i in spp..row.len() {
                    row[i] = row[i].wrapping_add(row[i - spp]);
                }
            }
            2 => undo_h16(row, spp),
            4 => undo_h32(row, spp),
            8 => undo_h64(row, spp),
            _ => {}
        }
    }
}

/// Apply horizontal differencing on each `row_bytes` row.
pub(crate) fn apply_horizontal(buf: &mut [u8], row_bytes: usize, spp: usize, bytes: usize) {
    if row_bytes == 0 {
        return;
    }
    for row in buf.chunks_mut(row_bytes) {
        match bytes {
            1 => {
                for i in (spp..row.len()).rev() {
                    row[i] = row[i].wrapping_sub(row[i - spp]);
                }
            }
            2 => do_h16(row, spp),
            4 => do_h32(row, spp),
            8 => do_h64(row, spp),
            _ => {}
        }
    }
}

/// Undo the floating-point predictor: byte accumulation with stride `spp`,
/// then reassemble the big-endian byte planes into native-endian samples.
pub(crate) fn undo_float(buf: &mut [u8], row_bytes: usize, spp: usize, bytes: usize) {
    if row_bytes == 0 || bytes == 0 {
        return;
    }
    let mut tmp = vec![0u8; row_bytes];
    for row in buf.chunks_mut(row_bytes) {
        let n = row.len();
        tmp[..n].copy_from_slice(row);
        for i in spp..n {
            tmp[i] = tmp[i].wrapping_add(tmp[i - spp]);
        }
        let wc = n / bytes;
        for k in 0..wc {
            for b in 0..bytes {
                // Plane b holds the b-th most significant byte.
                let v = tmp[b * wc + k];
                let dst = if cfg!(target_endian = "big") {
                    b
                } else {
                    bytes - 1 - b
                };
                row[k * bytes + dst] = v;
            }
        }
    }
}

/// Apply the floating-point predictor to native-endian samples.
pub(crate) fn apply_float(buf: &mut [u8], row_bytes: usize, spp: usize, bytes: usize) {
    if row_bytes == 0 || bytes == 0 {
        return;
    }
    let mut tmp = vec![0u8; row_bytes];
    for row in buf.chunks_mut(row_bytes) {
        let n = row.len();
        let wc = n / bytes;
        for k in 0..wc {
            for b in 0..bytes {
                let src = if cfg!(target_endian = "big") {
                    b
                } else {
                    bytes - 1 - b
                };
                tmp[b * wc + k] = row[k * bytes + src];
            }
        }
        for i in (spp..n).rev() {
            tmp[i] = tmp[i].wrapping_sub(tmp[i - spp]);
        }
        row.copy_from_slice(&tmp[..n]);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn round_trips() {
        for bytes in [1usize, 2, 4, 8] {
            for spp in [1usize, 3, 4] {
                let row_bytes = 7 * spp * bytes;
                let orig: Vec<u8> = (0..row_bytes * 3).map(|i| (i * 37 % 251) as u8).collect();
                let mut b = orig.clone();
                apply_horizontal(&mut b, row_bytes, spp, bytes);
                undo_horizontal(&mut b, row_bytes, spp, bytes);
                assert_eq!(b, orig);
                if bytes >= 2 {
                    let mut b = orig.clone();
                    apply_float(&mut b, row_bytes, spp, bytes);
                    undo_float(&mut b, row_bytes, spp, bytes);
                    assert_eq!(b, orig);
                }
            }
        }
    }
}
