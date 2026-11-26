//! SIMD twins of the HEIF layer's hot kernels: the quarter-turn rotation
//! (`irot` 90/270) and the YCbCr -> RGBA conversion.
//!
//! This is the only module allowed `unsafe`. Every kernel here sits behind a
//! safe function that checks the bounds it relies on, and every kernel has a
//! scalar twin that stays in the tree as its oracle and as the fallback on
//! other architectures and CPUs:
//!
//! | kernel | scalar oracle | gate |
//! |---|---|---|
//! | [`rotate_tiles`] (AVX2 runtime-detected, else SSE2) | `image::rotate_quarter_naive` | `assert_eq!` |
//! | [`rgba_row`] (AVX2, runtime-detected) | `rgb::to_rgba8_scalar` | `assert_eq!` |
//!
//! Both are exact, not float-close: the rotation moves samples, and the
//! RGBA kernel performs the scalar code's f32 operations in the same order
//! with no FMA (Rust never contracts `a * b + c`), so every lane rounds as
//! the scalar does.
//!
//! No aarch64 twins yet: on ARM both take the scalar code.
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
        //
        // With AVX2 the same tiles go two at a time (one per 128-bit lane);
        // the extra feature changes nothing about which addresses are read.
        let (sp, op) = (src.as_ptr(), out.as_mut_ptr());
        if std::is_x86_feature_detected!("avx2") {
            unsafe { x86::rotate_tiles_avx2(sp, w, h, q, op, nw, y0, ty0, ty1, tx0, tx1) }
        } else {
            unsafe { x86::rotate_tiles_sse2(sp, w, h, q, op, nw, y0, ty0, ty1, tx0, tx1) }
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

/// The per-picture constants of the YCbCr -> RGBA conversion, computed in
/// f32 exactly as `rgb::to_rgba8_scalar` computes them.
#[derive(Debug, Clone, Copy)]
pub(crate) struct RgbConsts {
    pub y_off: f32,
    pub y_scale: f32,
    pub c_scale: f32,
    pub mid: f32,
    /// `kr`, `kb`, `2(1-kr)`, `2(1-kb)` and `1-kr-kb`, each formed as the
    /// scalar expression forms it.
    pub kr: f32,
    pub kb: f32,
    pub kcr: f32,
    pub kcb: f32,
    pub kd: f32,
    /// `(1 << alpha_bit_depth) - 1`.
    pub amax: u32,
}

/// How the colour samples combine.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum RgbMode {
    /// `Kr`/`Kb` matrix.
    Matrix,
    /// `matrix_coefficients` 0: GBR.
    Identity,
    /// Luma only.
    Mono,
}

/// True when [`rgba_row`] runs vector code on this CPU.
pub(crate) fn rgba_simd_available() -> bool {
    #[cfg(target_arch = "x86_64")]
    {
        std::is_x86_feature_detected!("avx2")
    }
    #[cfg(not(target_arch = "x86_64"))]
    {
        false
    }
}

/// Convert the first `n` pixels of one row (`n` a multiple of 8) to
/// straight-alpha RGBA, into `out[..4 n]`. `cb`/`cr` are the chroma row at
/// horizontal shift `sx` (0 or 1; ignored for [`RgbMode::Mono`]), `alpha` the
/// alpha row (else alpha is 255). Premultiplied alpha is not handled here.
///
/// Call only when [`rgba_simd_available`] is true.
#[allow(clippy::too_many_arguments)]
pub(crate) fn rgba_row(
    c: &RgbConsts,
    mode: RgbMode,
    sx: u32,
    y: &[u16],
    cb: &[u16],
    cr: &[u16],
    alpha: Option<&[u16]>,
    out: &mut [u8],
    n: usize,
) {
    assert!(n % 8 == 0 && sx <= 1 && y.len() >= n && out.len() >= 4 * n);
    if mode != RgbMode::Mono {
        assert!(cb.len() >= n >> sx && cr.len() >= n >> sx);
    }
    if let Some(a) = alpha {
        assert!(a.len() >= n);
    }
    assert!(rgba_simd_available());
    #[cfg(target_arch = "x86_64")]
    {
        let (yp, cbp, crp, op) = (y.as_ptr(), cb.as_ptr(), cr.as_ptr(), out.as_mut_ptr());
        let ap = alpha.map(|a| a.as_ptr());
        // SAFETY: AVX2 is present (asserted). Group `g` of the kernel reads
        // luma and alpha `[8g, 8g+8)` and chroma `[8g >> sx, (8g >> sx) +
        // (8 >> sx))`, and writes `out[32g..32g+32]`; with `8g + 8 <= n`
        // these lie inside the lengths asserted above. Mono reads no chroma.
        unsafe {
            match (mode, sx, ap) {
                (RgbMode::Matrix, 1, None) => {
                    x86::rgba_avx2::<0, 1, false>(c, yp, cbp, crp, yp, op, n)
                }
                (RgbMode::Matrix, 1, Some(a)) => {
                    x86::rgba_avx2::<0, 1, true>(c, yp, cbp, crp, a, op, n)
                }
                (RgbMode::Matrix, _, None) => {
                    x86::rgba_avx2::<0, 0, false>(c, yp, cbp, crp, yp, op, n)
                }
                (RgbMode::Matrix, _, Some(a)) => {
                    x86::rgba_avx2::<0, 0, true>(c, yp, cbp, crp, a, op, n)
                }
                (RgbMode::Identity, 1, None) => {
                    x86::rgba_avx2::<1, 1, false>(c, yp, cbp, crp, yp, op, n)
                }
                (RgbMode::Identity, 1, Some(a)) => {
                    x86::rgba_avx2::<1, 1, true>(c, yp, cbp, crp, a, op, n)
                }
                (RgbMode::Identity, _, None) => {
                    x86::rgba_avx2::<1, 0, false>(c, yp, cbp, crp, yp, op, n)
                }
                (RgbMode::Identity, _, Some(a)) => {
                    x86::rgba_avx2::<1, 0, true>(c, yp, cbp, crp, a, op, n)
                }
                (RgbMode::Mono, _, None) => x86::rgba_avx2::<2, 0, false>(c, yp, yp, yp, yp, op, n),
                (RgbMode::Mono, _, Some(a)) => {
                    x86::rgba_avx2::<2, 0, true>(c, yp, yp, yp, a, op, n)
                }
            }
        }
    }
    #[cfg(not(target_arch = "x86_64"))]
    {
        let _ = (c, cb, cr);
        unreachable!("rgba_simd_available() is false off x86_64");
    }
}

/// The chroma half of [`rgba_row`] for one chroma row, widened to `n`
/// luma columns (`n` a multiple of 8): `tr[x]`/`tb[x]` are the terms the
/// kernel adds to luma for red and blue -- `2(1-kr) cr'` and `2(1-kb) cb'`
/// for [`RgbMode::Matrix`], `cr' + 0.5` and `cb' + 0.5` for
/// [`RgbMode::Identity`], where `c' = (c - mid) * c_scale` -- each formed
/// with the operations [`rgba_row`] uses. In 4:2:0 two luma rows share a
/// chroma row, so computing these once per chroma row and handing them to
/// [`rgba_row_terms`] does that work once instead of twice.
///
/// Call only when [`rgba_simd_available`] is true; not for mono.
#[allow(clippy::too_many_arguments)]
pub(crate) fn chroma_terms(
    c: &RgbConsts,
    mode: RgbMode,
    sx: u32,
    cb: &[u16],
    cr: &[u16],
    tr: &mut [f32],
    tb: &mut [f32],
    n: usize,
) {
    assert!(n % 8 == 0 && sx <= 1 && mode != RgbMode::Mono);
    assert!(cb.len() >= n >> sx && cr.len() >= n >> sx && tr.len() >= n && tb.len() >= n);
    assert!(rgba_simd_available());
    #[cfg(target_arch = "x86_64")]
    {
        let (cbp, crp, trp, tbp) = (cb.as_ptr(), cr.as_ptr(), tr.as_mut_ptr(), tb.as_mut_ptr());
        // SAFETY: AVX2 is present (asserted). Group `g` reads chroma
        // `[8g >> sx, (8g >> sx) + (8 >> sx))` and writes `tr`/`tb`
        // `[8g, 8g+8)`; with `8g + 8 <= n` all inside the asserted lengths.
        unsafe {
            match (mode, sx) {
                (RgbMode::Identity, 1) => x86::chroma_terms_avx2::<1, 1>(c, cbp, crp, trp, tbp, n),
                (RgbMode::Identity, _) => x86::chroma_terms_avx2::<1, 0>(c, cbp, crp, trp, tbp, n),
                (_, 1) => x86::chroma_terms_avx2::<0, 1>(c, cbp, crp, trp, tbp, n),
                (_, _) => x86::chroma_terms_avx2::<0, 0>(c, cbp, crp, trp, tbp, n),
            }
        }
    }
    #[cfg(not(target_arch = "x86_64"))]
    {
        let _ = (c, cb, cr, tr, tb);
        unreachable!("rgba_simd_available() is false off x86_64");
    }
}

/// [`rgba_row`] with the chroma terms already computed by
/// [`chroma_terms`]; same output. Not for mono.
#[allow(clippy::too_many_arguments)]
pub(crate) fn rgba_row_terms(
    c: &RgbConsts,
    mode: RgbMode,
    y: &[u16],
    tr: &[f32],
    tb: &[f32],
    alpha: Option<&[u16]>,
    out: &mut [u8],
    n: usize,
) {
    assert!(n % 8 == 0 && mode != RgbMode::Mono);
    assert!(y.len() >= n && tr.len() >= n && tb.len() >= n && out.len() >= 4 * n);
    if let Some(a) = alpha {
        assert!(a.len() >= n);
    }
    assert!(rgba_simd_available());
    #[cfg(target_arch = "x86_64")]
    {
        let (yp, trp, tbp, op) = (y.as_ptr(), tr.as_ptr(), tb.as_ptr(), out.as_mut_ptr());
        let ap = alpha.map(|a| a.as_ptr());
        // SAFETY: AVX2 is present (asserted). Group `g` reads luma, alpha,
        // `tr` and `tb` `[8g, 8g+8)` and writes `out[32g..32g+32]`; with
        // `8g + 8 <= n` all inside the asserted lengths.
        unsafe {
            match (mode, ap) {
                (RgbMode::Identity, None) => {
                    x86::rgba_terms_avx2::<1, false>(c, yp, trp, tbp, yp, op, n)
                }
                (RgbMode::Identity, Some(a)) => {
                    x86::rgba_terms_avx2::<1, true>(c, yp, trp, tbp, a, op, n)
                }
                (_, None) => x86::rgba_terms_avx2::<0, false>(c, yp, trp, tbp, yp, op, n),
                (_, Some(a)) => x86::rgba_terms_avx2::<0, true>(c, yp, trp, tbp, a, op, n),
            }
        }
    }
    #[cfg(not(target_arch = "x86_64"))]
    {
        let _ = (c, y, tr, tb, out);
        unreachable!("rgba_simd_available() is false off x86_64");
    }
}

#[cfg(target_arch = "x86_64")]
mod x86 {
    use std::arch::x86_64::*;

    use super::RgbConsts;

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

    /// [`transpose8`] on two independent tiles at once: `vpunpck*` works
    /// within each 128-bit lane, so tile A in the low lanes and tile B in the
    /// high lanes transpose with the same 24 instructions.
    #[inline(always)]
    unsafe fn transpose8x2(a: [__m256i; 8]) -> [__m256i; 8] {
        unsafe {
            let t0 = _mm256_unpacklo_epi16(a[0], a[1]);
            let t1 = _mm256_unpackhi_epi16(a[0], a[1]);
            let t2 = _mm256_unpacklo_epi16(a[2], a[3]);
            let t3 = _mm256_unpackhi_epi16(a[2], a[3]);
            let t4 = _mm256_unpacklo_epi16(a[4], a[5]);
            let t5 = _mm256_unpackhi_epi16(a[4], a[5]);
            let t6 = _mm256_unpacklo_epi16(a[6], a[7]);
            let t7 = _mm256_unpackhi_epi16(a[6], a[7]);
            let u0 = _mm256_unpacklo_epi32(t0, t2);
            let u1 = _mm256_unpackhi_epi32(t0, t2);
            let u2 = _mm256_unpacklo_epi32(t1, t3);
            let u3 = _mm256_unpackhi_epi32(t1, t3);
            let u4 = _mm256_unpacklo_epi32(t4, t6);
            let u5 = _mm256_unpackhi_epi32(t4, t6);
            let u6 = _mm256_unpacklo_epi32(t5, t7);
            let u7 = _mm256_unpackhi_epi32(t5, t7);
            [
                _mm256_unpacklo_epi64(u0, u4),
                _mm256_unpackhi_epi64(u0, u4),
                _mm256_unpacklo_epi64(u1, u5),
                _mm256_unpackhi_epi64(u1, u5),
                _mm256_unpacklo_epi64(u2, u6),
                _mm256_unpackhi_epi64(u2, u6),
                _mm256_unpacklo_epi64(u3, u7),
                _mm256_unpackhi_epi64(u3, u7),
            ]
        }
    }

    /// [`rotate_tiles_sse2`] two vertically adjacent output tiles at a
    /// time. Both tiles read the same eight source rows, at adjacent
    /// 8-column spans, so one 32-byte load per row feeds both (one per
    /// 128-bit lane) and the pair transposes in one pass of [`transpose8x2`].
    ///
    /// Why not leave it to SSE2: the SSE2 unpacks are destructive two-operand
    /// forms, so the compiler adds a `movdqa` before 16 of every 24; the VEX
    /// forms need none, and the pair halves the loads and unpacks per sample.
    /// Why vertical and not horizontal pairs: a horizontal pair reads sixteen
    /// source rows, and LLVM gave each row address its own induction variable
    /// and spilled them (about 40 stack moves per pair, measured); the
    /// vertical pair keeps the SSE2 kernel's eight. An odd last 8-row strip
    /// goes through the SSE2 kernel.
    ///
    /// # Safety
    /// AVX2, and the bounds documented at the call site in `rotate_tiles`
    /// (a pair is two tiles that each satisfy them).
    #[allow(clippy::too_many_arguments)]
    #[target_feature(enable = "avx2")]
    pub(super) unsafe fn rotate_tiles_avx2(
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
        let pairs_end = ty0 + (ty1 - ty0) / 16 * 16;
        let mut r0 = ty0;
        while r0 < pairs_end {
            let yn0 = y0 + r0;
            let (oa, ob) = unsafe { (out.add(r0 * nw), out.add((r0 + 8) * nw)) };
            let mut xn0 = tx0;
            while xn0 < tx1 {
                unsafe {
                    let mut rows = [_mm256_setzero_si256(); 8];
                    if q == 1 {
                        // Columns w-16-yn0 .. w-yn0: the low lane is the
                        // tile at r0+8, the high lane the tile at r0.
                        let base = src.add(xn0 * w + (w - 16 - yn0));
                        for (i, r) in rows.iter_mut().enumerate() {
                            *r = _mm256_loadu_si256(base.add(i * w) as *const __m256i);
                        }
                        let t = transpose8x2(rows);
                        for (k, v) in t.iter().rev().enumerate() {
                            let o = k * nw + xn0;
                            _mm_storeu_si128(ob.add(o) as *mut __m128i, _mm256_castsi256_si128(*v));
                            _mm_storeu_si128(
                                oa.add(o) as *mut __m128i,
                                _mm256_extracti128_si256(*v, 1),
                            );
                        }
                    } else {
                        // Columns yn0 .. yn0+16: the low lane is the tile at
                        // r0, the high lane the tile at r0+8.
                        let base = src.add((h - 1 - xn0) * w + yn0);
                        for (i, r) in rows.iter_mut().enumerate() {
                            *r = _mm256_loadu_si256(base.sub(i * w) as *const __m256i);
                        }
                        let t = transpose8x2(rows);
                        for (k, v) in t.iter().enumerate() {
                            let o = k * nw + xn0;
                            _mm_storeu_si128(oa.add(o) as *mut __m128i, _mm256_castsi256_si128(*v));
                            _mm_storeu_si128(
                                ob.add(o) as *mut __m128i,
                                _mm256_extracti128_si256(*v, 1),
                            );
                        }
                    }
                }
                xn0 += 8;
            }
            r0 += 16;
        }
        if pairs_end < ty1 {
            unsafe { rotate_tiles_sse2(src, w, h, q, out, nw, y0, pairs_end, ty1, tx0, tx1) }
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

    /// 8 `u16` -> 8 `f32`.
    #[inline(always)]
    unsafe fn load8_f32(p: *const u16) -> __m256 {
        unsafe { _mm256_cvtepi32_ps(_mm256_cvtepu16_epi32(_mm_loadu_si128(p as *const __m128i))) }
    }

    /// 4 `u16`, each duplicated -> 8 `f32` (nearest-neighbour 2:1 chroma).
    #[inline(always)]
    unsafe fn load4x2_f32(p: *const u16) -> __m256 {
        unsafe {
            let c = _mm_loadl_epi64(p as *const __m128i);
            _mm256_cvtepi32_ps(_mm256_cvtepu16_epi32(_mm_unpacklo_epi16(c, c)))
        }
    }

    /// `(v * 255 + 0.5).clamp(0, 255) as u8`, as `i32` lanes.
    #[inline(always)]
    unsafe fn to8(v: __m256) -> __m256i {
        unsafe {
            let t = _mm256_add_ps(_mm256_mul_ps(v, _mm256_set1_ps(255.0)), _mm256_set1_ps(0.5));
            let t = _mm256_min_ps(_mm256_max_ps(t, _mm256_setzero_ps()), _mm256_set1_ps(255.0));
            _mm256_cvttps_epi32(t)
        }
    }

    /// One row of YCbCr -> RGBA, `n` (a multiple of 8) pixels.
    ///
    /// `MODE`: 0 matrix, 1 identity, 2 mono. `SX`: chroma horizontal shift.
    /// `ALPHA`: read `a`, else alpha is 255.
    ///
    /// The alpha byte is `(a * 255 + amax / 2) / amax` in `u32`, truncated
    /// to 8 bits as the scalar `a8 as u8` is. The division is done in f32:
    /// with `x = a * 255 + amax / 2 < 2^24` (`a`, `amax` <= 65535) both `x`
    /// and `amax` are exact in f32, and the correctly rounded quotient of
    /// `x / amax` cannot reach the next integer: its rounding error is at
    /// most `q * 2^-24` while the distance to it is at least `1 / amax`, and
    /// `q * amax <= x < 2^24`. So truncating it is the integer quotient
    /// (checked for every `a` and every bit depth in the tests).
    ///
    /// # Safety
    /// AVX2; the bounds documented at the call site in `rgba_row`.
    #[allow(clippy::too_many_arguments)]
    #[target_feature(enable = "avx2")]
    pub(super) unsafe fn rgba_avx2<const MODE: u8, const SX: u32, const ALPHA: bool>(
        c: &RgbConsts,
        y: *const u16,
        cb: *const u16,
        cr: *const u16,
        a: *const u16,
        out: *mut u8,
        n: usize,
    ) {
        unsafe {
            let y_off = _mm256_set1_ps(c.y_off);
            let y_scale = _mm256_set1_ps(c.y_scale);
            let c_scale = _mm256_set1_ps(c.c_scale);
            let mid = _mm256_set1_ps(c.mid);
            let kr = _mm256_set1_ps(c.kr);
            let kb = _mm256_set1_ps(c.kb);
            let kcr = _mm256_set1_ps(c.kcr);
            let kcb = _mm256_set1_ps(c.kcb);
            let kd = _mm256_set1_ps(c.kd);
            let half = _mm256_set1_ps(0.5);
            let a255 = _mm256_set1_epi32(255);
            let ahalf = _mm256_set1_epi32((c.amax / 2) as i32);
            let amax = _mm256_set1_ps(c.amax as f32);
            let opaque = _mm256_set1_epi32(255 << 24);
            for g in 0..n / 8 {
                let x = g * 8;
                let yv = _mm256_mul_ps(_mm256_sub_ps(load8_f32(y.add(x)), y_off), y_scale);
                let (r, gg, b) = if MODE == 2 {
                    let v = to8(yv);
                    (v, v, v)
                } else {
                    let (cbv, crv) = if SX == 1 {
                        (load4x2_f32(cb.add(x / 2)), load4x2_f32(cr.add(x / 2)))
                    } else {
                        (load8_f32(cb.add(x)), load8_f32(cr.add(x)))
                    };
                    let cbv = _mm256_mul_ps(_mm256_sub_ps(cbv, mid), c_scale);
                    let crv = _mm256_mul_ps(_mm256_sub_ps(crv, mid), c_scale);
                    if MODE == 1 {
                        (
                            to8(_mm256_add_ps(crv, half)),
                            to8(yv),
                            to8(_mm256_add_ps(cbv, half)),
                        )
                    } else {
                        let r = _mm256_add_ps(yv, _mm256_mul_ps(kcr, crv));
                        let b = _mm256_add_ps(yv, _mm256_mul_ps(kcb, cbv));
                        let g = _mm256_div_ps(
                            _mm256_sub_ps(
                                _mm256_sub_ps(yv, _mm256_mul_ps(kr, r)),
                                _mm256_mul_ps(kb, b),
                            ),
                            kd,
                        );
                        (to8(r), to8(g), to8(b))
                    }
                };
                let av = if ALPHA {
                    let v = _mm256_cvtepu16_epi32(_mm_loadu_si128(a.add(x) as *const __m128i));
                    let num = _mm256_add_epi32(_mm256_mullo_epi32(v, a255), ahalf);
                    let q = _mm256_cvttps_epi32(_mm256_div_ps(_mm256_cvtepi32_ps(num), amax));
                    _mm256_slli_epi32(q, 24)
                } else {
                    opaque
                };
                let px = _mm256_or_si256(
                    _mm256_or_si256(r, _mm256_slli_epi32(gg, 8)),
                    _mm256_or_si256(_mm256_slli_epi32(b, 16), av),
                );
                _mm256_storeu_si256(out.add(x * 4) as *mut __m256i, px);
            }
        }
    }

    /// See [`super::chroma_terms`]. `MODE`: 0 matrix, 1 identity.
    ///
    /// # Safety
    /// AVX2; the bounds documented at the call site in `chroma_terms`.
    #[target_feature(enable = "avx2")]
    pub(super) unsafe fn chroma_terms_avx2<const MODE: u8, const SX: u32>(
        c: &RgbConsts,
        cb: *const u16,
        cr: *const u16,
        tr: *mut f32,
        tb: *mut f32,
        n: usize,
    ) {
        unsafe {
            let c_scale = _mm256_set1_ps(c.c_scale);
            let mid = _mm256_set1_ps(c.mid);
            let kcr = _mm256_set1_ps(c.kcr);
            let kcb = _mm256_set1_ps(c.kcb);
            let half = _mm256_set1_ps(0.5);
            for g in 0..n / 8 {
                let x = g * 8;
                let (cbv, crv) = if SX == 1 {
                    (load4x2_f32(cb.add(x / 2)), load4x2_f32(cr.add(x / 2)))
                } else {
                    (load8_f32(cb.add(x)), load8_f32(cr.add(x)))
                };
                let cbv = _mm256_mul_ps(_mm256_sub_ps(cbv, mid), c_scale);
                let crv = _mm256_mul_ps(_mm256_sub_ps(crv, mid), c_scale);
                let (r, b) = if MODE == 1 {
                    (_mm256_add_ps(crv, half), _mm256_add_ps(cbv, half))
                } else {
                    (_mm256_mul_ps(kcr, crv), _mm256_mul_ps(kcb, cbv))
                };
                _mm256_storeu_ps(tr.add(x), r);
                _mm256_storeu_ps(tb.add(x), b);
            }
        }
    }

    /// See [`super::rgba_row_terms`]: [`rgba_avx2`] with the chroma terms
    /// loaded instead of computed. `MODE`: 0 matrix, 1 identity.
    ///
    /// # Safety
    /// AVX2; the bounds documented at the call site in `rgba_row_terms`.
    #[allow(clippy::too_many_arguments)]
    #[target_feature(enable = "avx2")]
    pub(super) unsafe fn rgba_terms_avx2<const MODE: u8, const ALPHA: bool>(
        c: &RgbConsts,
        y: *const u16,
        tr: *const f32,
        tb: *const f32,
        a: *const u16,
        out: *mut u8,
        n: usize,
    ) {
        unsafe {
            let y_off = _mm256_set1_ps(c.y_off);
            let y_scale = _mm256_set1_ps(c.y_scale);
            let kr = _mm256_set1_ps(c.kr);
            let kb = _mm256_set1_ps(c.kb);
            let kd = _mm256_set1_ps(c.kd);
            let a255 = _mm256_set1_epi32(255);
            let ahalf = _mm256_set1_epi32((c.amax / 2) as i32);
            let amax = _mm256_set1_ps(c.amax as f32);
            let opaque = _mm256_set1_epi32(255 << 24);
            for g in 0..n / 8 {
                let x = g * 8;
                let yv = _mm256_mul_ps(_mm256_sub_ps(load8_f32(y.add(x)), y_off), y_scale);
                let trv = _mm256_loadu_ps(tr.add(x));
                let tbv = _mm256_loadu_ps(tb.add(x));
                let (r, gg, b) = if MODE == 1 {
                    (to8(trv), to8(yv), to8(tbv))
                } else {
                    let r = _mm256_add_ps(yv, trv);
                    let b = _mm256_add_ps(yv, tbv);
                    let g = _mm256_div_ps(
                        _mm256_sub_ps(
                            _mm256_sub_ps(yv, _mm256_mul_ps(kr, r)),
                            _mm256_mul_ps(kb, b),
                        ),
                        kd,
                    );
                    (to8(r), to8(g), to8(b))
                };
                let av = if ALPHA {
                    let v = _mm256_cvtepu16_epi32(_mm_loadu_si128(a.add(x) as *const __m128i));
                    let num = _mm256_add_epi32(_mm256_mullo_epi32(v, a255), ahalf);
                    let q = _mm256_cvttps_epi32(_mm256_div_ps(_mm256_cvtepi32_ps(num), amax));
                    _mm256_slli_epi32(q, 24)
                } else {
                    opaque
                };
                let px = _mm256_or_si256(
                    _mm256_or_si256(r, _mm256_slli_epi32(gg, 8)),
                    _mm256_or_si256(_mm256_slli_epi32(b, 16), av),
                );
                _mm256_storeu_si256(out.add(x * 4) as *mut __m256i, px);
            }
        }
    }
}
