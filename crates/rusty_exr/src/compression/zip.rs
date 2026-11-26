//! ZIP / ZIPS: byte-plane split + delta predictor, then zlib.
//!
//! Also home of the predictor/interleave pair that RLE and DWA reuse.

use crate::error::Result;

/// Undo the predictor in place: `t[i] = t[i-1] + t[i] - 128` (mod 256).
pub(crate) fn undo_predictor(t: &mut [u8]) {
    // A prefix sum over bytes. Bias every byte by -128 first, then
    // accumulate eight bytes at a time inside a u64 lane-wise (each byte is
    // the running sum of the bytes before it within the word, plus the
    // carry-in from the previous word broadcast to every lane). Byte lanes
    // never interact because every step is a lane-wise wrapping add.
    const L: u64 = 0x0101_0101_0101_0101;
    let n = t.len();
    if n == 0 {
        return;
    }
    let mut prev = t[0];
    let mut i = 1;
    // Head: process bytes until a word boundary is reached at index i.
    while i < n && (i - 1) % 8 != 0 {
        prev = prev.wrapping_add(t[i]).wrapping_sub(128);
        t[i] = prev;
        i += 1;
    }
    while i + 8 <= n {
        let mut w = u64::from_le_bytes([
            t[i],
            t[i + 1],
            t[i + 2],
            t[i + 3],
            t[i + 4],
            t[i + 5],
            t[i + 6],
            t[i + 7],
        ]);
        // d_k - 128 for each lane (lane-wise subtract of 0x80 == xor 0x80).
        w ^= 0x8080_8080_8080_8080;
        // In-word inclusive prefix sum over byte lanes (lane-wise adds via
        // masking so no carry crosses a lane).
        w = add_lanes(w, w << 8);
        w = add_lanes(w, w << 16);
        w = add_lanes(w, w << 32);
        // Add the carry-in (previous output byte) to every lane.
        w = add_lanes(w, u64::from(prev) * L);
        t[i..i + 8].copy_from_slice(&w.to_le_bytes());
        prev = (w >> 56) as u8;
        i += 8;
    }
    while i < n {
        prev = prev.wrapping_add(t[i]).wrapping_sub(128);
        t[i] = prev;
        i += 1;
    }
}

/// Lane-wise (8 x u8) wrapping add.
#[inline(always)]
fn add_lanes(a: u64, b: u64) -> u64 {
    const H: u64 = 0x8080_8080_8080_8080;
    ((a & !H).wrapping_add(b & !H)) ^ ((a ^ b) & H)
}

/// Apply the predictor in place (the inverse of [`undo_predictor`]).
pub(crate) fn apply_predictor(t: &mut [u8]) {
    for i in (1..t.len()).rev() {
        t[i] = t[i].wrapping_sub(t[i - 1]).wrapping_add(128);
    }
}

/// Re-interleave the two byte planes of `src` into `dst`:
/// `dst[2i] = src[i]`, `dst[2i+1] = src[(n+1)/2 + i]`.
pub(crate) fn interleave(src: &[u8], dst: &mut [u8]) {
    let n = src.len();
    let half = n.div_ceil(2);
    let (a, b) = src.split_at(half);
    for ((d, &x), &y) in dst.chunks_exact_mut(2).zip(a).zip(b) {
        d[0] = x;
        d[1] = y;
    }
    if n % 2 == 1 {
        dst[n - 1] = a[half - 1];
    }
}

/// Split `src` into its even bytes followed by its odd bytes.
pub(crate) fn deinterleave(src: &[u8], dst: &mut [u8]) {
    let n = src.len();
    let half = n.div_ceil(2);
    let (a, b) = dst.split_at_mut(half);
    for ((s, x), y) in src.chunks_exact(2).zip(a.iter_mut()).zip(b.iter_mut()) {
        *x = s[0];
        *y = s[1];
    }
    if n % 2 == 1 {
        a[half - 1] = src[n - 1];
    }
}

pub(crate) fn decompress(data: &[u8], raw_size: usize) -> Result<Vec<u8>> {
    let mut tmp = vec![0u8; raw_size];
    super::inflate_exact(data, &mut tmp)?;
    undo_predictor(&mut tmp);
    let mut out = vec![0u8; raw_size];
    interleave(&tmp, &mut out);
    Ok(out)
}

pub(crate) fn compress(raw: &[u8]) -> Vec<u8> {
    let mut tmp = vec![0u8; raw.len()];
    deinterleave(raw, &mut tmp);
    apply_predictor(&mut tmp);
    super::deflate(&tmp, 4)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn undo_scalar(t: &mut [u8]) {
        for i in 1..t.len() {
            t[i] = t[i - 1].wrapping_add(t[i]).wrapping_sub(128);
        }
    }

    #[test]
    fn predictor_matches_scalar() {
        let mut seed = 0x1234_5678u32;
        for n in 0..300 {
            let v: Vec<u8> = (0..n)
                .map(|_| {
                    seed = seed.wrapping_mul(1_103_515_245).wrapping_add(12345);
                    (seed >> 16) as u8
                })
                .collect();
            let mut a = v.clone();
            let mut b = v.clone();
            undo_predictor(&mut a);
            undo_scalar(&mut b);
            assert_eq!(a, b, "n = {n}");
            apply_predictor(&mut a);
            assert_eq!(a, v);
        }
    }

    #[test]
    fn interleave_round_trip() {
        for n in 0..20 {
            let v: Vec<u8> = (0..n as u8).collect();
            let mut s = vec![0; n];
            let mut back = vec![0; n];
            deinterleave(&v, &mut s);
            interleave(&s, &mut back);
            assert_eq!(v, back);
        }
    }
}
