// ---- block difference kernels ----------------------------------------------
//
// `encode_tx_block` and the filter-level search walk a block of source against
// a block of prediction/reconstruction several times over. These are the two
// shapes they need, as SSE2 (the x86_64 baseline, so no runtime dispatch) with
// a scalar twin that is the oracle and the fallback elsewhere.
//
// Samples are at most 12-bit (planes are clamped to `max_px`), so a `psubw`
// difference is exact in i16 and `pmaddwd(d, d)` — two squares per i32 lane,
// at most 2 * 4095^2 — cannot overflow. Lane sums are widened to u64 once per
// row (at most four adds per lane per row), which keeps that true at 32x32.

/// `out[y * bs + x] = src[y][x] - pred[y][x]` over a `bs x bs` block, returning
/// `Σ out²`. `bs` is 4, 8, 16 or 32.
#[inline]
pub(crate) fn residual_and_sse(
    src: &[u16],
    s_stride: usize,
    pred: &[u16],
    p_stride: usize,
    bs: usize,
    out: &mut [i32],
) -> u64 {
    #[cfg(target_arch = "x86_64")]
    {
        // One bounds proof for the whole block, then unchecked row loads.
        let out = &mut out[..bs * bs];
        let src = &src[..(bs - 1) * s_stride + bs];
        let pred = &pred[..(bs - 1) * p_stride + bs];
        // SAFETY: SSE2 is baseline on x86_64. Every load below reads `bs` (4 or a
        // multiple of 8) u16 at row offsets `y * stride`, `y < bs`, inside the
        // slices just taken; every store writes `bs` i32 inside `out`.
        unsafe { residual_and_sse_sse2(src.as_ptr(), s_stride, pred.as_ptr(), p_stride, bs, out) }
    }
    #[cfg(not(target_arch = "x86_64"))]
    {
        residual_and_sse_scalar(src, s_stride, pred, p_stride, bs, out)
    }
}

/// Scalar reference for [`residual_and_sse`].
#[cfg_attr(target_arch = "x86_64", allow(dead_code))]
pub(crate) fn residual_and_sse_scalar(
    src: &[u16],
    s_stride: usize,
    pred: &[u16],
    p_stride: usize,
    bs: usize,
    out: &mut [i32],
) -> u64 {
    let mut sse = 0u64;
    for y in 0..bs {
        let s_row = &src[y * s_stride..][..bs];
        let p_row = &pred[y * p_stride..][..bs];
        let d_row = &mut out[y * bs..][..bs];
        for x in 0..bs {
            let d = s_row[x] as i32 - p_row[x] as i32;
            d_row[x] = d;
            sse += (d as i64 * d as i64) as u64;
        }
    }
    sse
}

#[cfg(target_arch = "x86_64")]
#[inline]
unsafe fn residual_and_sse_sse2(
    src: *const u16,
    s_stride: usize,
    pred: *const u16,
    p_stride: usize,
    bs: usize,
    out: &mut [i32],
) -> u64 {
    use std::arch::x86_64::*;
    let o = out.as_mut_ptr();
    let zero = _mm_setzero_si128();
    let mut acc64 = zero;
    if bs == 4 {
        let mut acc = zero;
        for y in 0..4 {
            let s = _mm_loadl_epi64(src.add(y * s_stride) as *const __m128i);
            let p = _mm_loadl_epi64(pred.add(y * p_stride) as *const __m128i);
            let d = _mm_sub_epi16(s, p);
            _mm_storeu_si128(
                o.add(y * 4) as *mut __m128i,
                _mm_srai_epi32::<16>(_mm_unpacklo_epi16(d, d)),
            );
            acc = _mm_add_epi32(acc, _mm_madd_epi16(d, d));
        }
        acc64 = _mm_add_epi64(_mm_unpacklo_epi32(acc, zero), _mm_unpackhi_epi32(acc, zero));
    } else {
        for y in 0..bs {
            let (sr, pr, or) = (src.add(y * s_stride), pred.add(y * p_stride), o.add(y * bs));
            let mut acc = zero;
            let mut x = 0;
            while x < bs {
                let s = _mm_loadu_si128(sr.add(x) as *const __m128i);
                let p = _mm_loadu_si128(pr.add(x) as *const __m128i);
                let d = _mm_sub_epi16(s, p);
                _mm_storeu_si128(
                    or.add(x) as *mut __m128i,
                    _mm_srai_epi32::<16>(_mm_unpacklo_epi16(d, d)),
                );
                _mm_storeu_si128(
                    or.add(x + 4) as *mut __m128i,
                    _mm_srai_epi32::<16>(_mm_unpackhi_epi16(d, d)),
                );
                acc = _mm_add_epi32(acc, _mm_madd_epi16(d, d));
                x += 8;
            }
            acc64 = _mm_add_epi64(acc64, _mm_unpacklo_epi32(acc, zero));
            acc64 = _mm_add_epi64(acc64, _mm_unpackhi_epi32(acc, zero));
        }
    }
    let hi = _mm_unpackhi_epi64(acc64, acc64);
    _mm_cvtsi128_si64(_mm_add_epi64(acc64, hi)) as u64
}

/// `Σ (a[i] - b[i])²` over two equal-length sample runs — the whole-frame SSE
/// the filter-level search takes per candidate.
///
/// A separate out-of-line symbol on purpose: [`block_sse`] is `inline(always)`
/// for the per-transform-block call site, and when this frame-sized use called
/// it directly LLVM stopped inlining it there too, which cost the block path a
/// call per transform block (+0.1% on an encode that never runs the search).
#[inline(never)]
pub(crate) fn run_sse(a: &[u16], b: &[u16]) -> u64 {
    let n = a.len().min(b.len());
    block_sse(&a[..n], n, &b[..n], n, n, 1)
}

/// `Σ (a[y][x] - b[y][x])²` over a `w x h` block. `w` is 4 or a multiple of 8.
#[inline(always)]
pub(crate) fn block_sse(
    a: &[u16],
    a_stride: usize,
    b: &[u16],
    b_stride: usize,
    w: usize,
    h: usize,
) -> u64 {
    #[cfg(target_arch = "x86_64")]
    if w == 4 || w % 8 == 0 {
        if h == 0 {
            return 0;
        }
        let a = &a[..(h - 1) * a_stride + w];
        let b = &b[..(h - 1) * b_stride + w];
        // SAFETY: SSE2 is baseline on x86_64; every load reads `w` u16 at row
        // offsets `y * stride`, `y < h`, inside the slices just taken.
        return unsafe { block_sse_sse2(a.as_ptr(), a_stride, b.as_ptr(), b_stride, w, h) };
    }
    block_sse_scalar(a, a_stride, b, b_stride, w, h)
}

/// Scalar reference for [`block_sse`].
pub(crate) fn block_sse_scalar(
    a: &[u16],
    a_stride: usize,
    b: &[u16],
    b_stride: usize,
    w: usize,
    h: usize,
) -> u64 {
    let mut sse = 0u64;
    for y in 0..h {
        let a_row = &a[y * a_stride..][..w];
        let b_row = &b[y * b_stride..][..w];
        for x in 0..w {
            let d = a_row[x] as i64 - b_row[x] as i64;
            sse += (d * d) as u64;
        }
    }
    sse
}

#[cfg(target_arch = "x86_64")]
#[inline]
unsafe fn block_sse_sse2(
    a: *const u16,
    a_stride: usize,
    b: *const u16,
    b_stride: usize,
    w: usize,
    h: usize,
) -> u64 {
    use std::arch::x86_64::*;
    let zero = _mm_setzero_si128();
    let mut acc64 = zero;
    for y in 0..h {
        let (ar, br) = (a.add(y * a_stride), b.add(y * b_stride));
        let mut acc = zero;
        if w == 4 {
            let d = _mm_sub_epi16(
                _mm_loadl_epi64(ar as *const __m128i),
                _mm_loadl_epi64(br as *const __m128i),
            );
            acc = _mm_madd_epi16(d, d);
        } else {
            let mut x = 0;
            while x < w {
                let d = _mm_sub_epi16(
                    _mm_loadu_si128(ar.add(x) as *const __m128i),
                    _mm_loadu_si128(br.add(x) as *const __m128i),
                );
                acc = _mm_add_epi32(acc, _mm_madd_epi16(d, d));
                x += 8;
                // Keep the i32 lanes from overflowing on very wide rows: 32 adds
                // of at most 2 * 4095^2 per lane is the proven-safe run.
                if x % 256 == 0 {
                    acc64 = _mm_add_epi64(acc64, _mm_unpacklo_epi32(acc, zero));
                    acc64 = _mm_add_epi64(acc64, _mm_unpackhi_epi32(acc, zero));
                    acc = zero;
                }
            }
        }
        acc64 = _mm_add_epi64(acc64, _mm_unpacklo_epi32(acc, zero));
        acc64 = _mm_add_epi64(acc64, _mm_unpackhi_epi32(acc, zero));
    }
    let hi = _mm_unpackhi_epi64(acc64, acc64);
    _mm_cvtsi128_si64(_mm_add_epi64(acc64, hi)) as u64
}

#[cfg(test)]
mod blockkern_tests {
    use super::*;

    fn xs(s: &mut u64) -> u64 {
        *s ^= *s << 13;
        *s ^= *s >> 7;
        *s ^= *s << 17;
        *s
    }

    /// Both kernels against their scalar twins: every block size, odd strides,
    /// 8/10/12-bit ranges, and the worst case (all samples at opposite extremes).
    #[test]
    fn block_kernels_match_scalar() {
        let mut s = 0x0123_4567_89ab_cdefu64;
        for &max in &[255u64, 1023, 4095] {
            for &bs in &[4usize, 8, 16, 32] {
                for round in 0..60 {
                    let (ss, ps) = (bs + (round % 5), bs + 3 * (round % 3));
                    let mut src = vec![0u16; ss * bs + 8];
                    let mut pred = vec![0u16; ps * bs + 8];
                    for v in src.iter_mut() {
                        *v = match round {
                            0 => max as u16,
                            1 => 0,
                            _ => (xs(&mut s) % (max + 1)) as u16,
                        };
                    }
                    for v in pred.iter_mut() {
                        *v = match round {
                            0 => 0,
                            1 => max as u16,
                            _ => (xs(&mut s) % (max + 1)) as u16,
                        };
                    }
                    let mut want = vec![0i32; bs * bs];
                    let mut got = vec![7i32; bs * bs];
                    let ws = residual_and_sse_scalar(&src, ss, &pred, ps, bs, &mut want);
                    let gs = residual_and_sse(&src, ss, &pred, ps, bs, &mut got);
                    assert_eq!(got, want, "residual bs={bs} max={max} round={round}");
                    assert_eq!(gs, ws, "residual sse bs={bs} max={max} round={round}");
                    assert_eq!(
                        block_sse(&src, ss, &pred, ps, bs, bs),
                        block_sse_scalar(&src, ss, &pred, ps, bs, bs),
                        "block_sse bs={bs} max={max} round={round}"
                    );
                }
            }
            // A wide, short strip (the filter-search shape) at the extremes.
            let (w, h) = (1024usize, 3usize);
            let a = vec![max as u16; w * h];
            let b = vec![0u16; w * h];
            assert_eq!(block_sse(&a, w, &b, w, w, h), block_sse_scalar(&a, w, &b, w, w, h));
            assert_eq!(block_sse(&a, w, &b, w, w, h), (max * max) * (w * h) as u64);
        }
    }
}
