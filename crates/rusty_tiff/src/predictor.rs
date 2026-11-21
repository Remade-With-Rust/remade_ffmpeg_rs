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

hpred!(undo_h16, do_h16_tail, u16, 2);
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

/// 8-bit forward differencing in place: `row[i] -= row[i - spp]`, back to
/// front so every byte read is still an original one.
///
/// Blocks of 32 from the end: a block loads its own bytes and the bytes
/// `spp` to their left (all still original: only bytes right of the block
/// have been written), subtracts and stores. With both operands in
/// fixed-size arrays the subtraction is two vector instructions; the
/// byte-at-a-time loop, with its two bounds checks, was 7 Ir a byte.
fn do_h8(row: &mut [u8], spp: usize) {
    const B: usize = 32;
    let mut i = row.len();
    while i >= spp + B {
        let cur: [u8; B] = row[i - B..i].try_into().unwrap();
        let prev: [u8; B] = row[i - B - spp..i - spp].try_into().unwrap();
        let mut d = [0u8; B];
        for k in 0..B {
            d[k] = cur[k].wrapping_sub(prev[k]);
        }
        row[i - B..i].copy_from_slice(&d);
        i -= B;
    }
    for j in (spp..i).rev() {
        row[j] = row[j].wrapping_sub(row[j - spp]);
    }
}

/// The same for native-endian 16-bit samples, 16 samples a block (the
/// sample-at-a-time loop was 12 Ir a sample); `do_h16_tail` finishes the
/// front of the row.
fn do_h16(row: &mut [u8], spp: usize) {
    const B: usize = 32;
    let stride = spp * 2;
    let mut i = row.len() - row.len() % 2;
    while i >= stride + B {
        let cur: [u8; B] = row[i - B..i].try_into().unwrap();
        let prev: [u8; B] = row[i - B - stride..i - stride].try_into().unwrap();
        let mut d = [0u8; B];
        for ((d, c), p) in d
            .chunks_exact_mut(2)
            .zip(cur.chunks_exact(2))
            .zip(prev.chunks_exact(2))
        {
            let v = u16::from_ne_bytes([c[0], c[1]]).wrapping_sub(u16::from_ne_bytes([p[0], p[1]]));
            d.copy_from_slice(&v.to_ne_bytes());
        }
        row[i - B..i].copy_from_slice(&d);
        i -= B;
    }
    do_h16_tail(&mut row[..i], spp);
}

/// Apply horizontal differencing on each `row_bytes` row.
pub(crate) fn apply_horizontal(buf: &mut [u8], row_bytes: usize, spp: usize, bytes: usize) {
    if row_bytes == 0 {
        return;
    }
    // The width is matched once per chunk, not once per row.
    let rows = buf.chunks_mut(row_bytes);
    match bytes {
        1 => rows.for_each(|row| do_h8(row, spp)),
        2 => rows.for_each(|row| do_h16(row, spp)),
        4 => rows.for_each(|row| do_h32(row, spp)),
        8 => rows.for_each(|row| do_h64(row, spp)),
        _ => {}
    }
}

/// Byte-wise running sum with stride `spp` (`row[i] += row[i - spp]`), with
/// the per-sample accumulators of [`undo_h8`] for 1 to 4 samples per pixel
/// (the indexed loop, which re-reads `row[i - spp]` through a bounds check,
/// cost 4.3 Ir a byte more: 17.8% of the instructions of an uncompressed
/// float decode and 37% of its time).
fn accumulate8(row: &mut [u8], spp: usize) {
    match spp {
        1 => undo_h8::<1>(row),
        2 => undo_h8::<2>(row),
        3 => undo_h8::<3>(row),
        4 => undo_h8::<4>(row),
        _ => {
            for i in spp..row.len() {
                row[i] = row[i].wrapping_add(row[i - spp]);
            }
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
        accumulate8(&mut tmp, spp);
        let wc = n / bytes;
        // Plane b holds the b-th most significant byte of every sample.
        // The planes are zipped per sample width, which vectorises; one
        // indexed store per byte through a run-time width was 13.4 Ir a
        // byte (67% of an uncompressed 32-bit float decode).
        match bytes {
            2 => {
                let (p0, p1) = tmp.split_at(wc);
                for ((d, &a), &b) in row.chunks_exact_mut(2).zip(p0).zip(p1) {
                    d.copy_from_slice(&u16::from_be_bytes([a, b]).to_ne_bytes());
                }
            }
            4 => {
                let (p0, r) = tmp.split_at(wc);
                let (p1, r) = r.split_at(wc);
                let (p2, p3) = r.split_at(wc);
                let px = row.chunks_exact_mut(4);
                for ((((d, &a), &b), &c), &e) in px.zip(p0).zip(p1).zip(p2).zip(p3) {
                    d.copy_from_slice(&u32::from_be_bytes([a, b, c, e]).to_ne_bytes());
                }
            }
            _ => {
                for k in 0..wc {
                    for b in 0..bytes {
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
    }
}

/// Append the `N` byte planes of `row`'s native-endian `N`-byte samples to
/// `tmp`, most significant byte first: one pass per plane, each a fixed
/// stride over the row.
fn split_planes<const N: usize>(row: &[u8], tmp: &mut Vec<u8>) {
    for b in 0..N {
        let src = if cfg!(target_endian = "big") {
            b
        } else {
            N - 1 - b
        };
        tmp.extend(row.chunks_exact(N).map(|s| s[src]));
    }
}

/// Apply the floating-point predictor to native-endian samples.
pub(crate) fn apply_float(buf: &mut [u8], row_bytes: usize, spp: usize, bytes: usize) {
    if row_bytes == 0 || bytes == 0 {
        return;
    }
    // The planes are appended to an empty buffer (nothing is zero-filled
    // per call, and a call is one strip) by a per-width pass; one scattered
    // store per byte through a run-time width was 13 Ir a byte.
    let mut tmp = Vec::with_capacity(row_bytes);
    for row in buf.chunks_mut(row_bytes) {
        let n = row.len();
        tmp.clear();
        match bytes {
            2 => split_planes::<2>(row, &mut tmp),
            4 => split_planes::<4>(row, &mut tmp),
            8 => split_planes::<8>(row, &mut tmp),
            _ => {
                let wc = n / bytes;
                tmp.resize(wc * bytes, 0);
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
            }
        }
        // (A row cut inside a sample: the cut bytes count as zero.)
        tmp.resize(n, 0);
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

    /// The float predictor's definition, one byte at a time (the oracle for
    /// the per-width kernels).
    fn ref_undo_float(buf: &mut [u8], row_bytes: usize, spp: usize, bytes: usize) {
        for row in buf.chunks_mut(row_bytes) {
            let n = row.len();
            let mut tmp = row.to_vec();
            for i in spp..n {
                tmp[i] = tmp[i].wrapping_add(tmp[i - spp]);
            }
            let wc = n / bytes;
            for k in 0..wc {
                for b in 0..bytes {
                    let dst = if cfg!(target_endian = "big") {
                        b
                    } else {
                        bytes - 1 - b
                    };
                    row[k * bytes + dst] = tmp[b * wc + k];
                }
            }
        }
    }

    fn ref_apply_float(buf: &mut [u8], row_bytes: usize, spp: usize, bytes: usize) {
        for row in buf.chunks_mut(row_bytes) {
            let n = row.len();
            let mut tmp = vec![0u8; n];
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
            row.copy_from_slice(&tmp);
        }
    }

    #[test]
    fn float_predictor_matches_the_definition() {
        let mut seed = 99u32;
        for bytes in [2usize, 4, 8] {
            for spp in 1..=5usize {
                // Whole rows, and a buffer whose last row is cut inside a
                // sample (the callers never pass one; the kernels must still
                // agree with the definition there).
                for (px, cut) in [
                    (1usize, 0usize),
                    (2, 0),
                    (3, 0),
                    (7, 0),
                    (16, 0),
                    (33, 5),
                    (40, 1),
                ] {
                    let row_bytes = px * spp * bytes;
                    let orig: Vec<u8> = (0..row_bytes * 3 - cut)
                        .map(|_| {
                            seed = seed.wrapping_mul(1_103_515_245).wrapping_add(12345);
                            (seed >> 16) as u8
                        })
                        .collect();
                    let (mut a, mut b) = (orig.clone(), orig.clone());
                    undo_float(&mut a, row_bytes, spp, bytes);
                    ref_undo_float(&mut b, row_bytes, spp, bytes);
                    assert_eq!(a, b, "undo: bytes {bytes} spp {spp} px {px}");
                    let (mut a, mut b) = (orig.clone(), orig.clone());
                    apply_float(&mut a, row_bytes, spp, bytes);
                    ref_apply_float(&mut b, row_bytes, spp, bytes);
                    assert_eq!(a, b, "apply: bytes {bytes} spp {spp} px {px}");
                }
            }
        }
    }

    /// Horizontal differencing by its definition: sample by sample, back
    /// to front (the oracle for the block kernels).
    fn ref_apply_horizontal(buf: &mut [u8], row_bytes: usize, spp: usize, bytes: usize) {
        let big = cfg!(target_endian = "big");
        let rd = |b: &[u8]| {
            let mut a = [0u8; 8];
            if big {
                a[8 - b.len()..].copy_from_slice(b);
            } else {
                a[..b.len()].copy_from_slice(b);
            }
            u64::from_ne_bytes(a)
        };
        for row in buf.chunks_mut(row_bytes) {
            let stride = spp * bytes;
            let mut i = row.len() - row.len() % bytes;
            while i >= stride + bytes {
                i -= bytes;
                let v =
                    rd(&row[i..i + bytes]).wrapping_sub(rd(&row[i - stride..i - stride + bytes]));
                let a = v.to_ne_bytes();
                let a = if big { &a[8 - bytes..] } else { &a[..bytes] };
                row[i..i + bytes].copy_from_slice(a);
            }
        }
    }

    #[test]
    fn forward_horizontal_matches_the_definition() {
        let mut seed = 77u32;
        for bytes in [1usize, 2, 4, 8] {
            for spp in 1..=5usize {
                for px in [1usize, 2, 5, 16, 17, 31, 32, 33, 64, 100] {
                    let row_bytes = px * spp * bytes;
                    // Whole rows, and a short last row cut inside a sample.
                    for cut in [0usize, 1, 3] {
                        let orig: Vec<u8> = (0..(row_bytes * 3).saturating_sub(cut))
                            .map(|_| {
                                seed = seed.wrapping_mul(1_103_515_245).wrapping_add(12345);
                                (seed >> 16) as u8
                            })
                            .collect();
                        let (mut a, mut b) = (orig.clone(), orig.clone());
                        apply_horizontal(&mut a, row_bytes, spp, bytes);
                        ref_apply_horizontal(&mut b, row_bytes, spp, bytes);
                        assert_eq!(a, b, "bytes {bytes} spp {spp} px {px} cut {cut}");
                        undo_horizontal(&mut a, row_bytes, spp, bytes);
                        assert_eq!(a, orig, "undo: bytes {bytes} spp {spp} px {px} cut {cut}");
                    }
                }
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
