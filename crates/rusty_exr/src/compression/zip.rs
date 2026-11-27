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
/// The SIMD twin handles a suffix of 16-byte steps; this loop does the
/// rest and is the oracle.
pub(crate) fn apply_predictor(t: &mut [u8]) {
    let from = crate::simd::apply_predictor(crate::simd::isa(), t);
    apply_predictor_to(t, from);
}

/// [`apply_predictor`] over `t[..end]` (`t[end..]` already done; `end`
/// at most `t.len()`).
pub(crate) fn apply_predictor_to(t: &mut [u8], end: usize) {
    for i in (1..end).rev() {
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

/// Split `src` into its even bytes followed by its odd bytes. The SIMD
/// twin handles a prefix of 16-pair steps; this loop does the rest and is
/// the oracle.
pub(crate) fn deinterleave(src: &[u8], dst: &mut [u8]) {
    let done = crate::simd::deinterleave(crate::simd::isa(), src, dst);
    deinterleave_from(src, dst, done);
}

/// [`deinterleave`] from byte pair `pairs_done` on.
pub(crate) fn deinterleave_from(src: &[u8], dst: &mut [u8], pairs_done: usize) {
    let n = src.len();
    let half = n.div_ceil(2);
    let (a, b) = dst.split_at_mut(half);
    let pairs = src.chunks_exact(2).zip(a.iter_mut()).zip(b.iter_mut());
    for ((s, x), y) in pairs.skip(pairs_done) {
        *x = s[0];
        *y = s[1];
    }
    if n % 2 == 1 {
        a[half - 1] = src[n - 1];
    }
}

/// Byte-plane split, then the predictor: what the ZIP and RLE writers
/// compress, into a reused buffer (every byte is written).
pub(crate) fn prepare_into<'a>(raw: &[u8], buf: &'a mut Vec<u8>) -> &'a [u8] {
    let tmp = super::grow(buf, raw.len());
    deinterleave(raw, tmp);
    apply_predictor(tmp);
    tmp
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

pub(crate) fn compress(raw: &[u8], deflater: crate::Deflate, work: &mut super::Work) -> Vec<u8> {
    super::deflate(prepare_into(raw, &mut work.prep), deflater)
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
    fn encoder_twins_match_scalar() {
        let mut seed = 0x7777_1111u32;
        for n in (0..200).chain([255, 256, 257, 1000, 4097]) {
            let v: Vec<u8> = (0..n)
                .map(|_| {
                    seed = seed.wrapping_mul(1_103_515_245).wrapping_add(12345);
                    (seed >> 16) as u8
                })
                .collect();
            let mut want_d = vec![0u8; n];
            deinterleave_from(&v, &mut want_d, 0);
            let mut want_p = want_d.clone();
            apply_predictor_to(&mut want_p, n);
            for isa in crate::simd::available() {
                let mut d = vec![0u8; n];
                let done = crate::simd::deinterleave(isa, &v, &mut d);
                deinterleave_from(&v, &mut d, done);
                assert_eq!(d, want_d, "{isa:?} deinterleave n {n}");
                let from = crate::simd::apply_predictor(isa, &mut d);
                assert!(from <= n.max(1));
                apply_predictor_to(&mut d, from);
                assert_eq!(d, want_p, "{isa:?} predictor n {n}");
            }
            // A reused buffer holding stale, longer data gives the same bytes.
            let mut buf = vec![0xa5; n + 7];
            assert_eq!(prepare_into(&v, &mut buf), &want_p[..]);
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
