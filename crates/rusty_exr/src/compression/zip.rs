//! ZIP / ZIPS: byte-plane split + delta predictor, then zlib.
//!
//! Also home of the predictor/interleave pair that RLE and DWA reuse.

use crate::error::Result;

/// Undo the predictor in place: `t[i] = t[i-1] + t[i] - 128` (mod 256).
///
/// A byte prefix sum, eight bytes per step. Within a word the even and odd
/// bytes are spread into 16-bit lanes, where four bytes can never overflow a
/// lane, so one multiply by `0x0001_0001_0001_0001` forms all the lane
/// prefix sums at once; the running byte from the previous word is added at
/// the end. Only that final add sits on the loop-carried path.
///
/// The SIMD twin (`simd::undo_predictor`) handles a prefix of whole
/// 16-byte steps; this word loop finishes the rest and is the oracle.
pub(crate) fn undo_predictor(t: &mut [u8]) {
    let done = crate::simd::undo_predictor(crate::simd::isa(), t);
    undo_predictor_from(t, done);
}

/// [`undo_predictor`] over `t[start..]`, where `t[..start]` is already
/// done (`start == 0` does the whole buffer).
pub(crate) fn undo_predictor_from(t: &mut [u8], start: usize) {
    const M: u64 = 0x00ff_00ff_00ff_00ff;
    const SPLAT16: u64 = 0x0001_0001_0001_0001;
    if t.is_empty() {
        return;
    }
    let start = start.max(1);
    let mut prev = t[start - 1];
    let rest = &mut t[start..];
    let mut words = rest.chunks_exact_mut(8);
    for c in &mut words {
        // d_k - 128 (mod 256) in every byte.
        let w = u64::from_le_bytes([c[0], c[1], c[2], c[3], c[4], c[5], c[6], c[7]])
            ^ 0x8080_8080_8080_8080;
        let even = w & M;
        let odd = (w >> 8) & M;
        // Lane i of `pre` = sum of bytes 0..=2i+1.
        let pre = (even + odd).wrapping_mul(SPLAT16);
        let p = u64::from(prev) * SPLAT16;
        let out_odd = (pre + p) & M;
        let out_even = (pre.wrapping_sub(odd) + p) & M;
        let out = out_even | (out_odd << 8);
        c.copy_from_slice(&out.to_le_bytes());
        prev = (out >> 56) as u8;
    }
    for v in words.into_remainder() {
        prev = prev.wrapping_add(*v).wrapping_sub(128);
        *v = prev;
    }
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

/// Inflate and undo the predictor into `scratch.a`, which then holds the
/// raw layout's even bytes followed by its odd bytes.
pub(crate) fn decompress_halves(
    data: &[u8],
    raw_size: usize,
    scratch: &mut super::Scratch,
) -> Result<()> {
    scratch.inflate_a(data, raw_size)?;
    undo_predictor(&mut scratch.a);
    Ok(())
}

pub(crate) fn compress(raw: &[u8], deflater: crate::Deflate) -> Vec<u8> {
    let mut tmp = vec![0u8; raw.len()];
    deinterleave(raw, &mut tmp);
    apply_predictor(&mut tmp);
    super::deflate(&tmp, deflater)
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
            let mut c = v.clone();
            undo_predictor_from(&mut c, 0);
            assert_eq!(c, b, "word loop, n = {n}");
            for isa in crate::simd::available() {
                let mut d = v.clone();
                let done = crate::simd::undo_predictor(isa, &mut d);
                assert!(done <= n);
                undo_predictor_from(&mut d, done);
                assert_eq!(d, b, "{isa:?}, n = {n}");
            }
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
