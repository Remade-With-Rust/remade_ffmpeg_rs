//! SIMD twins of the HEIF layer's hot kernels: the quarter-turn rotation
//! (`irot` 90/270).
//!
//! This is the only module allowed `unsafe`. Every kernel here sits behind a
//! safe function that checks the bounds it relies on, and every kernel has a
//! scalar twin that stays in the tree as its oracle and as the fallback on
//! other architectures and CPUs:
//!
//! | kernel | scalar oracle | gate |
//! |---|---|---|
//! | [`rotate_tiles`] (SSE2) | `image::rotate_quarter_naive` | `assert_eq!` |
//!
//! The rotation is exact by construction: it moves samples.
//!
//! No aarch64 twins yet: on ARM the scalar code runs.
#![allow(unsafe_code)]

/// Rotate the `8x8` tiles of output rows `[ty0, ty1)` x columns `[tx0, tx1)`
/// of a quarter turn (`q` 1 = anti-clockwise, 3 = clockwise) of the
/// `w`x`h` plane `src`. `out` holds the output rows starting at global row
/// `y0`, `nw` (= `h`) samples each; `ty0`/`ty1` are rows of `out`. Both
/// ranges must be whole tiles.
///
/// Each tile is eight 16-byte source loads, a 24-unpack transpose and eight
/// 16-byte stores, where the scalar loop does a strided load and a store per
/// sample. The tile's source rows are loaded in the order that makes the
/// transpose land directly in output order, so no lane is ever reversed:
///
/// * `q` = 1: `new[yn][xn] = old[xn][w-1-yn]` -- output row `k` of the tile
///   is transposed row `7-k`;
/// * `q` = 3: `new[yn][xn] = old[h-1-xn][yn]` -- the source rows are loaded
///   bottom-up, and output row `k` is transposed row `k`.
#[allow(clippy::too_many_arguments)]
pub(crate) fn rotate_tiles(
    src: &[u16],
    w: usize,
    h: usize,
    q: u8,
    out: &mut [u16],
    nw: usize,
    y0: usize,
    (ty0, ty1): (usize, usize),
    (tx0, tx1): (usize, usize),
) {
    // Every index the kernel forms is checked here once.
    assert!(nw == h && src.len() >= w * h && out.len() >= ty1 * nw);
    assert!((ty1 - ty0) % 8 == 0 && (tx1 - tx0) % 8 == 0);
    assert!(tx1 <= nw && y0 + ty1 <= w && (q == 1 || q == 3));
    #[cfg(target_arch = "x86_64")]
    {
        // SAFETY: SSE2 is part of the x86_64 baseline. Bounds: a tile at
        // output (yn0 = y0 + r0, xn0) reads source rows `xn0..xn0+8` (q 1)
        // or `h-8-xn0..h-xn0` (q 3) -- inside `0..h` because
        // `xn0 + 8 <= tx1 <= nw = h` -- at columns `w-8-yn0..w-yn0` (q 1) or
        // `yn0..yn0+8` (q 3) -- inside `0..w` because
        // `yn0 + 8 <= y0 + ty1 <= w`; so every load is inside
        // `src[..w * h]`. It writes rows `r0..r0+8 <= ty1` of `out`, columns
        // `xn0..xn0+8 <= nw`: inside `out[..ty1 * nw]`. All asserted above.
        unsafe {
            x86::rotate_tiles_sse2(
                src.as_ptr(),
                w,
                h,
                q,
                out.as_mut_ptr(),
                nw,
                y0,
                ty0,
                ty1,
                tx0,
                tx1,
            )
        }
    }
    #[cfg(not(target_arch = "x86_64"))]
    for r in ty0..ty1 {
        let yn = y0 + r;
        for xn in tx0..tx1 {
            out[r * nw + xn] = if q == 1 {
                src[xn * w + (w - 1 - yn)]
            } else {
                src[(h - 1 - xn) * w + yn]
            };
        }
    }
}

#[cfg(target_arch = "x86_64")]
mod x86 {
    use std::arch::x86_64::*;

    /// Transpose eight rows of eight `u16`: `out[j][i] = rows[i][j]`.
    #[inline(always)]
    unsafe fn transpose8(a: [__m128i; 8]) -> [__m128i; 8] {
        unsafe {
            let t0 = _mm_unpacklo_epi16(a[0], a[1]);
            let t1 = _mm_unpackhi_epi16(a[0], a[1]);
            let t2 = _mm_unpacklo_epi16(a[2], a[3]);
            let t3 = _mm_unpackhi_epi16(a[2], a[3]);
            let t4 = _mm_unpacklo_epi16(a[4], a[5]);
            let t5 = _mm_unpackhi_epi16(a[4], a[5]);
            let t6 = _mm_unpacklo_epi16(a[6], a[7]);
            let t7 = _mm_unpackhi_epi16(a[6], a[7]);
            let u0 = _mm_unpacklo_epi32(t0, t2);
            let u1 = _mm_unpackhi_epi32(t0, t2);
            let u2 = _mm_unpacklo_epi32(t1, t3);
            let u3 = _mm_unpackhi_epi32(t1, t3);
            let u4 = _mm_unpacklo_epi32(t4, t6);
            let u5 = _mm_unpackhi_epi32(t4, t6);
            let u6 = _mm_unpacklo_epi32(t5, t7);
            let u7 = _mm_unpackhi_epi32(t5, t7);
            [
                _mm_unpacklo_epi64(u0, u4),
                _mm_unpackhi_epi64(u0, u4),
                _mm_unpacklo_epi64(u1, u5),
                _mm_unpackhi_epi64(u1, u5),
                _mm_unpacklo_epi64(u2, u6),
                _mm_unpackhi_epi64(u2, u6),
                _mm_unpacklo_epi64(u3, u7),
                _mm_unpackhi_epi64(u3, u7),
            ]
        }
    }

    /// See [`super::rotate_tiles`].
    ///
    /// # Safety
    /// The bounds documented at the call site in `rotate_tiles`.
    #[allow(clippy::too_many_arguments)]
    #[target_feature(enable = "sse2")]
    pub(super) unsafe fn rotate_tiles_sse2(
        src: *const u16,
        w: usize,
        h: usize,
        q: u8,
        out: *mut u16,
        nw: usize,
        y0: usize,
        ty0: usize,
        ty1: usize,
        tx0: usize,
        tx1: usize,
    ) {
        let mut r0 = ty0;
        while r0 < ty1 {
            let yn0 = y0 + r0;
            let mut xn0 = tx0;
            while xn0 < tx1 {
                unsafe {
                    let mut rows = [_mm_setzero_si128(); 8];
                    if q == 1 {
                        let base = src.add(xn0 * w + (w - 8 - yn0));
                        for (i, r) in rows.iter_mut().enumerate() {
                            *r = _mm_loadu_si128(base.add(i * w) as *const __m128i);
                        }
                        let t = transpose8(rows);
                        let o = out.add(r0 * nw + xn0);
                        for (k, v) in t.iter().rev().enumerate() {
                            _mm_storeu_si128(o.add(k * nw) as *mut __m128i, *v);
                        }
                    } else {
                        let base = src.add((h - 1 - xn0) * w + yn0);
                        for (i, r) in rows.iter_mut().enumerate() {
                            *r = _mm_loadu_si128(base.sub(i * w) as *const __m128i);
                        }
                        let t = transpose8(rows);
                        let o = out.add(r0 * nw + xn0);
                        for (k, v) in t.iter().enumerate() {
                            _mm_storeu_si128(o.add(k * nw) as *mut __m128i, *v);
                        }
                    }
                }
                xn0 += 8;
            }
            r0 += 8;
        }
    }
}
