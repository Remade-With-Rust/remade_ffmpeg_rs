//! Predictors 2 (horizontal differencing) and 3 (floating-point byte-plane
//! differencing), forward and inverse, plus byte-order helpers.
//!
//! All functions work row by row on byte buffers whose multi-byte samples are
//! in NATIVE byte order, except the float predictor's coded form, which is
//! big-endian byte planes by definition (libtiff `fpAcc`/`fpDiff`).

/// Reverse the byte order of every `n`-byte sample in place.
///
/// The sample width is fixed per arm, so each loop is a load, a byte swap
/// and a store that the compiler vectorises. One loop over a run-time width
/// (`chunks_exact_mut(n)` + `reverse`) cost 20 Ir per 16-bit sample: 87% of
/// a big-endian 16-bit decode, which went from 107.9M to 21.8M Ir (and from
/// 17.7 to 5.6 ms).
pub(crate) fn swap_bytes(buf: &mut [u8], n: usize) {
    fn swap<const N: usize>(buf: &mut [u8]) {
        for c in buf.chunks_exact_mut(N) {
            let c: &mut [u8; N] = c.try_into().unwrap();
            c.reverse();
        }
    }
    match n {
        0 | 1 => {}
        2 => swap::<2>(buf),
        4 => swap::<4>(buf),
        8 => swap::<8>(buf),
        _ => {
            for c in buf.chunks_exact_mut(n) {
                c.reverse();
            }
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
        match (bytes, spp) {
            (1, 1) => undo_h8::<1>(row),
            (1, 2) => undo_h8::<2>(row),
            (1, 3) => undo_h8::<3>(row),
            (1, 4) => undo_h8::<4>(row),
            (1, _) => {
                for i in spp..row.len() {
                    row[i] = row[i].wrapping_add(row[i - spp]);
                }
            }
            (2, 1) => undo_h16n::<1>(row),
            (2, 3) => undo_h16n::<3>(row),
            (2, 4) => undo_h16n::<4>(row),
            (2, _) => undo_h16(row, spp),
            (4, _) => undo_h32(row, spp),
            (8, _) => undo_h64(row, spp),
            _ => {}
        }
    }
}

/// 8-bit horizontal predictor for `N` samples per pixel: a running per-
/// sample accumulator in a fixed-size array instead of `row[i - spp]`
/// re-reads with a bounds check per byte. A trailing partial pixel (row
/// bytes not a multiple of `N`) is added to the last pixel, as the generic
/// loop does.
fn undo_h8<const N: usize>(row: &mut [u8]) {
    let mut px = row.chunks_exact_mut(N);
    let Some(first) = px.next() else { return };
    let mut acc: [u8; N] = (&*first).try_into().unwrap();
    for p in &mut px {
        for k in 0..N {
            acc[k] = acc[k].wrapping_add(p[k]);
            p[k] = acc[k];
        }
    }
    for (b, a) in px.into_remainder().iter_mut().zip(acc) {
        *b = b.wrapping_add(a);
    }
}

/// The same for native-endian 16-bit samples.
fn undo_h16n<const N: usize>(row: &mut [u8]) {
    let mut px = row.chunks_exact_mut(2 * N);
    let Some(first) = px.next() else { return };
    let mut acc = [0u16; N];
    for k in 0..N {
        acc[k] = u16::from_ne_bytes([first[2 * k], first[2 * k + 1]]);
    }
    for p in &mut px {
        for k in 0..N {
            acc[k] = acc[k].wrapping_add(u16::from_ne_bytes([p[2 * k], p[2 * k + 1]]));
            p[2 * k..2 * k + 2].copy_from_slice(&acc[k].to_ne_bytes());
        }
    }
    // A trailing partial pixel: whole samples only, as the generic loop.
    let rem = px.into_remainder();
    for (k, s) in rem.chunks_exact_mut(2).enumerate() {
        let v = u16::from_ne_bytes([s[0], s[1]]).wrapping_add(acc[k]);
        s.copy_from_slice(&v.to_ne_bytes());
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
    // The row copy is appended to an empty buffer: `vec![0; row_bytes]`
    // zero-filled a row per call, and a call is one strip (often one row):
    // 4.4% of the instructions of an uncompressed float decode.
    let mut tmp = Vec::with_capacity(row_bytes);
    for row in buf.chunks_mut(row_bytes) {
        let n = row.len();
        tmp.clear();
        tmp.extend_from_slice(row);
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
    fn swap_bytes_reverses_each_sample() {
        for n in [0usize, 1, 2, 3, 4, 8] {
            // Lengths with a trailing partial sample, which stays as it is.
            for len in [0usize, 1, 7, 16, 37, 64, 67] {
                let orig: Vec<u8> = (0..len).map(|i| (i * 29 + 3) as u8).collect();
                let mut want = orig.clone();
                if n > 1 {
                    for c in want.chunks_exact_mut(n) {
                        c.reverse();
                    }
                }
                let mut got = orig.clone();
                swap_bytes(&mut got, n);
                assert_eq!(got, want, "n {n} len {len}");
            }
        }
    }

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
