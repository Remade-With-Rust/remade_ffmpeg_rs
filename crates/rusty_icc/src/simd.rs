//! x86-64 AVX2 kernels: the only `unsafe` in the crate.
//!
//! Rules every kernel here follows (the audit list is in the pull request
//! that added the module, and in `README.md`):
//!
//! - A kernel is a safe `#[target_feature(enable = "avx2")]` function. The
//!   `unsafe` is confined to (a) the one call into it from a public entry
//!   point, made only after `is_x86_feature_detected!("avx2")` said yes, and
//!   (b) the raw-pointer loads, stores and gathers in the small helpers
//!   below, each bounded by a fixed-size array reference or by an index clamp
//!   written on the line before it.
//! - Every kernel has a scalar twin: the code it stands in for, which stays
//!   in the tree as the oracle of the tests and as the path on every other
//!   CPU and target. In exact mode the kernel and its twin agree bit for bit
//!   (the same IEEE operations in the same order, lane by lane: no FMA, no
//!   reassociation).
//! - An entry point returns how many pixels it did (a multiple of 8, from the
//!   front); the caller runs the scalar twin on the rest. On targets other
//!   than x86-64, and on x86-64 CPUs without AVX2, every entry point returns
//!   0 and the scalar twin does everything.

use crate::fast::{FastChain, FastShaperView};
use crate::transform::{Sample, OUT_TABLE};

/// The tables and matrix of a three-channel matrix-shaper transform
/// (`ShaperEngine` in `transform.rs`), borrowed for one call: input tables of
/// `N` entries (256 for 8-bit, 65536 for 16-bit samples), one per input
/// channel, the 3x3 matrix and offset, and the `sqrt`-indexed output tables.
// Read only by the x86-64 kernels.
#[cfg_attr(not(target_arch = "x86_64"), allow(dead_code))]
pub(crate) struct ShaperTables<'a, const N: usize> {
    pub(crate) rows: [&'a [f32; N]; 3],
    pub(crate) outs: [&'a [f32; OUT_TABLE]; 3],
    pub(crate) m: [[f32; 3]; 3],
    pub(crate) off: [f32; 3],
}

/// One output channel's exact 8-bit codes by threshold (`U8Codes` in
/// `transform.rs`), borrowed for one call.
#[derive(Clone, Copy)]
#[cfg_attr(not(target_arch = "x86_64"), allow(dead_code))]
pub(crate) struct Codes8<'a> {
    pub(crate) table: &'a [u32],
    pub(crate) base: i32,
    pub(crate) shift: u32,
}

/// Pixels the kernels have done, for the reachability census of the tests
/// (counted once per call, never per pixel; test builds only).
#[cfg(test)]
pub(crate) static CENSUS: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);

#[cfg(test)]
#[cfg_attr(not(target_arch = "x86_64"), allow(dead_code))]
fn count(done: usize) {
    CENSUS.fetch_add(done as u64, std::sync::atomic::Ordering::Relaxed);
}

#[cfg(not(test))]
#[cfg_attr(not(target_arch = "x86_64"), allow(dead_code))]
#[inline(always)]
fn count(_: usize) {}

/// `true` when this CPU runs the AVX2 kernels.
#[inline]
pub(crate) fn avx2() -> bool {
    #[cfg(target_arch = "x86_64")]
    {
        std::is_x86_feature_detected!("avx2")
    }
    #[cfg(not(target_arch = "x86_64"))]
    {
        false
    }
}

/// The 8-bit matrix-shaper loop (`ShaperEngine::run_loop::<u8, 3, 3>`) on
/// the leading multiple of 8 pixels: interleaved RGB or RGBA in and out,
/// alpha copied (`alpha.0 && alpha.1`) or set opaque as the scalar loop
/// does. With `codes` (one per output channel, each table non-empty) the
/// output tables and the quantisation are replaced by the threshold lookup,
/// which gives the same codes. Returns the pixels done.
pub(crate) fn shaper_u8(
    t: &ShaperTables<'_, 256>,
    codes: Option<&[Codes8<'_>; 3]>,
    src: &[u8],
    dst: &mut [u8],
    (sc, dc): (usize, usize),
    alpha: (bool, bool),
) -> usize {
    #[cfg(target_arch = "x86_64")]
    {
        let codes = codes.filter(|c| c.iter().all(|c| !c.table.is_empty() && c.shift < 32));
        if matches!((sc, dc), (3 | 4, 3 | 4)) && avx2() {
            // SAFETY: AVX2 was detected on this CPU on the line above; the
            // kernel has no other precondition (it bounds its own reads and
            // writes by the slices it is given, and `codes` tables are
            // non-empty, checked on the line before).
            let done = unsafe { x86::shaper_u8(t, codes, src, dst, (sc, dc), alpha.0 && alpha.1) };
            count(done);
            return done;
        }
    }
    let _ = (t, codes, src, dst, sc, dc, alpha);
    0
}

/// The fast `f32` matrix-shaper loop ([`FastShaperView::run`]) on the
/// leading multiple of 8 pixels: the same values, bit for bit. Returns the
/// pixels done.
pub(crate) fn shaper_f32_fast(
    t: &FastShaperView<'_>,
    src: &[f32],
    dst: &mut [f32],
    (sc, dc): (usize, usize),
    alpha: (bool, bool),
    near: &mut Vec<usize>,
) -> usize {
    #[cfg(target_arch = "x86_64")]
    {
        if matches!((sc, dc), (3 | 4, 3 | 4)) && avx2() {
            // SAFETY: as in `shaper_u8`: AVX2 detected just above.
            let done =
                unsafe { x86::shaper_f32_fast(t, src, dst, (sc, dc), alpha.0 && alpha.1, near) };
            count(done);
            return done;
        }
    }
    let _ = (t, src, dst, sc, dc, alpha, near);
    0
}

/// The fast `f32` LUT-chain loop ([`FastChain::run_scalar`]) on the leading
/// pixels: the same values, bit for bit. Returns the pixels done.
pub(crate) fn chain_f32_fast(
    f: &FastChain,
    src: &[f32],
    dst: &mut [f32],
    (sc, dc): (usize, usize),
    alpha: (bool, bool),
) -> usize {
    #[cfg(target_arch = "x86_64")]
    {
        let (i, o) = f.channels();
        if matches!((sc, dc), (3 | 4, 3 | 4)) && sc >= i && dc >= o && avx2() {
            // SAFETY: as in `shaper_u8`: AVX2 detected just above.
            let done = unsafe { x86::chain_f32_fast(f, src, dst, (sc, dc), alpha.0 && alpha.1) };
            count(done);
            return done;
        }
    }
    let _ = (f, src, dst, sc, dc, alpha);
    0
}

/// [`shaper_u8`] for 16-bit samples.
pub(crate) fn shaper_u16(
    t: &ShaperTables<'_, 65536>,
    src: &[u16],
    dst: &mut [u16],
    (sc, dc): (usize, usize),
    alpha: (bool, bool),
) -> usize {
    #[cfg(target_arch = "x86_64")]
    {
        if matches!((sc, dc), (3 | 4, 3 | 4)) && avx2() {
            // SAFETY: as in `shaper_u8`: AVX2 detected just above.
            let done = unsafe { x86::shaper_u16(t, src, dst, (sc, dc), alpha.0 && alpha.1) };
            count(done);
            return done;
        }
    }
    let _ = (t, src, dst, sc, dc, alpha);
    0
}

/// The integer device-link grid loop for three inputs and four outputs
/// (`grid_fixed::<T, 3, 4, _>` in `transform.rs`, whose lookup is
/// `ClutEval::tetra_select`): RGB or RGBA (`sc` 3 or 4) 8- or 16-bit
/// samples in, CMYK without alpha out. `cube` is the grid, `g` points per
/// axis, row-major with the first input slowest and four values per point;
/// `cell(sample)` is the sample's `(cell, fraction)` on an axis (cell below
/// `g`). Every pixel is done (the loop has no lanes to fill), bit for bit
/// as the scalar loop does it; returns the pixels done, 0 when the shape or
/// the CPU does not fit (the scalar loop then does them).
pub(crate) fn grid34<T: Sample, C: Fn(T) -> (usize, f64)>(
    cube: &[f32],
    g: usize,
    cell: C,
    src: &[T],
    dst: &mut [T],
    (sc, dc): (usize, usize),
    dst_alpha: bool,
) -> usize {
    #[cfg(target_arch = "x86_64")]
    {
        let fits = (2..=4096).contains(&g)
            && g.checked_pow(3).and_then(|n| n.checked_mul(4)) == Some(cube.len());
        if fits && matches!(sc, 3 | 4) && dc == 4 && !dst_alpha && avx2() {
            let Some(cube) = x86::Cube::new(cube, g) else {
                return 0;
            };
            use crate::transform::SampleKind;
            let done = match T::KIND {
                SampleKind::U16 => match T::u16s_mut(dst) {
                    // SAFETY: AVX2 was detected on this CPU (the `if`
                    // above); the kernel bounds its own reads and writes
                    // (by the slices it is given and the `Cube`'s length).
                    Some(d) => unsafe { x86::grid34_u16(&cube, &cell, src, sc, d) },
                    None => 0,
                },
                SampleKind::U8 => match T::u8s_mut(dst) {
                    // SAFETY: as for the 16-bit arm: AVX2 detected above.
                    Some(d) => unsafe { x86::grid34_u8(&cube, &cell, src, sc, d) },
                    None => 0,
                },
                SampleKind::F32 => 0,
            };
            count(done);
            return done;
        }
    }
    let _ = (cube, g, cell, src, dst, sc, dc, dst_alpha);
    0
}

/// Test hook: the grid kernel's lookup at `cells` (its four `f64` lanes)
/// and its quantisation of `q` to 16 and 8 bits, for the `f64`-level
/// oracle tests (the output-level ones cannot see a reassociated sum or a
/// flipped tie: 16-bit quantisation hides a last-bit difference). `None`
/// without AVX2 or for a grid `grid34` declines.
#[cfg(test)]
pub(crate) fn grid34_parts(
    cube: &[f32],
    g: usize,
    cells: [(usize, f64); 3],
    q: [f64; 4],
) -> Option<([f64; 4], [i32; 4], [i32; 4])> {
    #[cfg(target_arch = "x86_64")]
    if avx2() {
        let c = x86::Cube::new(cube, g)?;
        // SAFETY: AVX2 detected on the line above.
        return Some(unsafe { x86::grid34_parts(&c, cells, q) });
    }
    let _ = (cube, g, cells, q);
    None
}

#[cfg(target_arch = "x86_64")]
mod x86 {
    use super::{Codes8, FastChain, FastShaperView, ShaperTables, OUT_TABLE};
    use crate::fast::{
        FastClut, FastLutIn, FastLutOut, LabF, Table1, LABF_BASE, LABF_CELLS, LABF_LIMIT,
        LABF_SHIFT, LABF_TOP,
    };
    use crate::fast::{FastCurve, CELLS, FAST_TABLE, GUARD};
    use core::arch::x86_64::*;

    // ---- memory helpers: every raw-pointer access of the module ----

    /// Sixteen bytes from a 16-byte array.
    #[inline]
    #[target_feature(enable = "avx2")]
    fn load128(p: &[u8; 16]) -> __m128i {
        // SAFETY: `p` refers to 16 readable bytes; the load is unaligned.
        unsafe { _mm_loadu_si128(p.as_ptr().cast()) }
    }

    /// Eight bytes from an 8-byte array, in the low half (upper half zero).
    #[inline]
    #[target_feature(enable = "avx2")]
    fn load64(p: &[u8; 8]) -> __m128i {
        // SAFETY: `p` refers to 8 readable bytes; `movq` reads exactly 8.
        unsafe { _mm_loadl_epi64(p.as_ptr().cast()) }
    }

    /// Thirty-two bytes from a 32-byte array.
    #[inline]
    #[target_feature(enable = "avx2")]
    fn load256(p: &[u8; 32]) -> __m256i {
        // SAFETY: `p` refers to 32 readable bytes; the load is unaligned.
        unsafe { _mm256_loadu_si256(p.as_ptr().cast()) }
    }

    #[inline]
    #[target_feature(enable = "avx2")]
    fn store128(p: &mut [u8; 16], v: __m128i) {
        // SAFETY: `p` refers to 16 writable bytes; the store is unaligned.
        unsafe { _mm_storeu_si128(p.as_mut_ptr().cast(), v) }
    }

    /// Eight 16-bit samples.
    #[inline]
    #[target_feature(enable = "avx2")]
    fn load_words(p: &[u16; 8]) -> __m128i {
        // SAFETY: `p` refers to 16 readable bytes; the load is unaligned.
        unsafe { _mm_loadu_si128(p.as_ptr().cast()) }
    }

    #[inline]
    #[target_feature(enable = "avx2")]
    fn store_words(p: &mut [u16; 8], v: __m128i) {
        // SAFETY: `p` refers to 16 writable bytes; the store is unaligned.
        unsafe { _mm_storeu_si128(p.as_mut_ptr().cast(), v) }
    }

    #[inline]
    #[target_feature(enable = "avx2")]
    fn store64(p: &mut [u8; 8], v: __m128i) {
        // SAFETY: `p` refers to 8 writable bytes; `movq` writes exactly 8.
        unsafe { _mm_storel_epi64(p.as_mut_ptr().cast(), v) }
    }

    #[inline]
    #[target_feature(enable = "avx2")]
    fn store256(p: &mut [u8; 32], v: __m256i) {
        // SAFETY: `p` refers to 32 writable bytes; the store is unaligned.
        unsafe { _mm256_storeu_si256(p.as_mut_ptr().cast(), v) }
    }

    /// `table[idx & (N - 1)]` per lane, for a power-of-two `N` (every
    /// caller's indices are already in range, so the mask changes nothing
    /// for them; it is what bounds the reads).
    #[inline]
    #[target_feature(enable = "avx2")]
    fn gather<const N: usize>(table: &[f32; N], idx: __m256i) -> __m256 {
        const { assert!(N.is_power_of_two() && N <= 1 << 30) };
        let idx = _mm256_and_si256(idx, _mm256_set1_epi32((N - 1) as i32));
        // SAFETY: every lane of `idx` is in 0..=N-1 (masked on the line
        // above), so each 4-byte read at `table + 4 * idx` lies inside the
        // N `f32`s `table` refers to.
        unsafe { _mm256_i32gather_ps::<4>(table.as_ptr(), idx) }
    }

    /// `table[idx]` per lane, for a table of at least one entry and `idx`
    /// lanes that are non-negative (the caller's `max` with 0): clamped to
    /// the table's last entry here.
    #[inline]
    #[target_feature(enable = "avx2")]
    fn gather_u32(table: &[u32], idx: __m256i) -> __m256i {
        let Some(top) = table.len().checked_sub(1) else {
            return _mm256_setzero_si256();
        };
        let top = top.min(i32::MAX as usize) as i32;
        let idx = _mm256_min_epu32(idx, _mm256_set1_epi32(top));
        // SAFETY: `table` is non-empty (`checked_sub` above) and every lane
        // of `idx`, read as unsigned, is at most `len - 1` (the unsigned
        // `min` above; `top` saturating at i32::MAX only shrinks the range),
        // so each 4-byte read lies inside it.
        unsafe { _mm256_i32gather_epi32::<4>(table.as_ptr().cast(), idx) }
    }

    /// A `pshufb` control from 16 bytes.
    #[inline]
    #[target_feature(enable = "avx2")]
    fn mask(m: [u8; 16]) -> __m128i {
        load128(&m)
    }

    /// `p[..N]` as an array reference (the callers pass exact chunks, so the
    /// test folds away).
    #[inline(always)]
    fn arr<const N: usize>(p: &[u8]) -> &[u8; N] {
        p.first_chunk::<N>()
            .expect("chunk of the kernel's own width")
    }

    #[inline(always)]
    fn arr_mut<const N: usize>(p: &mut [u8]) -> &mut [u8; N] {
        p.first_chunk_mut::<N>()
            .expect("chunk of the kernel's own width")
    }

    // ---- shuffle controls (built at compile time) ----

    const Z: u8 = 0x80;

    /// Byte `k` of channel `c` (of 3) from interleaved 8-bit RGB, for the
    /// 16-byte block `j` of the source: the source byte `3k + c` when it is
    /// in that block, else zero. Bytes 8..16 of the result are zero.
    const fn rgb8_in(c: usize, j: usize) -> [u8; 16] {
        let mut m = [Z; 16];
        let mut k = 0;
        while k < 8 {
            let b = 3 * k + c;
            if b / 16 == j {
                m[k] = (b % 16) as u8;
            }
            k += 1;
        }
        m
    }

    /// The four 32-bit lanes `0x00BBGGRR` of one 128-bit half to 12 packed
    /// bytes `RGBRGBRGBRGB` (bytes 12..16 zero).
    const RGB8_OUT: [u8; 16] = [0, 1, 2, 4, 5, 6, 8, 9, 10, 12, 13, 14, Z, Z, Z, Z];

    /// Word `k` of channel `c` (of 3) from interleaved 16-bit RGB, for the
    /// 16-byte block `j` (words `8j..8j+8`) of the source.
    const fn rgb16_in(c: usize, j: usize) -> [u8; 16] {
        let mut m = [Z; 16];
        let mut k = 0;
        while k < 8 {
            let w = 3 * k + c;
            if w / 8 == j {
                m[2 * k] = (2 * (w % 8)) as u8;
                m[2 * k + 1] = (2 * (w % 8) + 1) as u8;
            }
            k += 1;
        }
        m
    }

    /// For output block `j` (words `8j..8j+8` of interleaved 16-bit RGB):
    /// the words that come from channel `c`'s eight packed words.
    const fn rgb16_out(c: usize, j: usize) -> [u8; 16] {
        let mut m = [Z; 16];
        let mut w = 0;
        while w < 8 {
            let at = 8 * j + w;
            if at % 3 == c {
                let k = at / 3;
                m[2 * w] = (2 * k) as u8;
                m[2 * w + 1] = (2 * k + 1) as u8;
            }
            w += 1;
        }
        m
    }

    // ---- the matrix-shaper core ----

    /// The colour values of eight pixels before quantisation: per output
    /// channel `ShaperEngine::lookup_out(out[o], m[o] . lin + off[o])` with
    /// `lin[c] = rows[c][idx[c]]`, the scalar loop's operations in its
    /// order, lane by lane.
    #[inline]
    #[target_feature(enable = "avx2")]
    fn mix<const N: usize>(t: &ShaperTables<'_, N>, idx: [__m256i; 3]) -> [__m256; 3] {
        let l0 = gather(t.rows[0], idx[0]);
        let l1 = gather(t.rows[1], idx[1]);
        let l2 = gather(t.rows[2], idx[2]);
        product(&t.m, &t.off, [l0, l1, l2])
    }

    /// Per output channel `o`: `m[o][0] * l[0] + m[o][1] * l[1] +
    /// m[o][2] * l[2] + off[o]`, left to right.
    #[inline]
    #[target_feature(enable = "avx2")]
    fn product(m: &[[f32; 3]; 3], off: &[f32; 3], [l0, l1, l2]: [__m256; 3]) -> [__m256; 3] {
        let mut v = [_mm256_setzero_ps(); 3];
        for (o, v) in v.iter_mut().enumerate() {
            let r = m[o];
            // `r[0] * l0 + r[1] * l1 + r[2] * l2 + off`, left to right.
            *v = _mm256_add_ps(
                _mm256_add_ps(
                    _mm256_add_ps(
                        _mm256_mul_ps(_mm256_set1_ps(r[0]), l0),
                        _mm256_mul_ps(_mm256_set1_ps(r[1]), l1),
                    ),
                    _mm256_mul_ps(_mm256_set1_ps(r[2]), l2),
                ),
                _mm256_set1_ps(off[o]),
            );
        }
        v
    }

    /// `if v > 0 { v.min(1) } else { 0 }`: `maxps` returns its second
    /// operand when the first is NaN and for two zeros, so NaN and -0.0
    /// become +0.0 as in the scalar form.
    #[inline]
    #[target_feature(enable = "avx2")]
    fn clamp01(v: __m256) -> __m256 {
        _mm256_min_ps(_mm256_max_ps(v, _mm256_setzero_ps()), _mm256_set1_ps(1.0))
    }

    /// The 8-bit codes of `v` by threshold (`U8Codes::lookup`): bucket and
    /// in-bucket position from the clamped value's bit pattern, the entry's
    /// first code plus one, minus one when the position is below its step.
    #[inline]
    #[target_feature(enable = "avx2")]
    fn codes8(v: __m256, c: &Codes8<'_>) -> __m256i {
        let bits = _mm256_castps_si256(clamp01(v));
        let off = _mm256_max_epi32(
            _mm256_sub_epi32(bits, _mm256_set1_epi32(c.base)),
            _mm256_setzero_si256(),
        );
        let shift = _mm_cvtsi32_si128(c.shift as i32);
        let e = gather_u32(c.table, _mm256_srl_epi32(off, shift));
        let low = _mm256_and_si256(off, _mm256_set1_epi32(((1u32 << c.shift) - 1) as i32));
        let step = _mm256_and_si256(e, _mm256_set1_epi32(0x7F_FFFF));
        // `cmpgt` is -1 where the step is above the position.
        _mm256_add_epi32(_mm256_srli_epi32::<23>(e), _mm256_cmpgt_epi32(step, low))
    }

    #[inline]
    #[target_feature(enable = "avx2")]
    fn shade<const N: usize>(t: &ShaperTables<'_, N>, idx: [__m256i; 3]) -> [__m256; 3] {
        let v = mix(t, idx);
        let scale = _mm256_set1_ps((OUT_TABLE - 1) as f32);
        let last = _mm256_set1_epi32((OUT_TABLE - 2) as i32);
        let step = _mm256_set1_epi32(1);
        let mut y = [_mm256_setzero_ps(); 3];
        for (o, y) in y.iter_mut().enumerate() {
            let c = clamp01(v[o]);
            let pos = _mm256_mul_ps(_mm256_sqrt_ps(c), scale);
            let i = _mm256_min_epi32(_mm256_cvttps_epi32(pos), last);
            let f = _mm256_sub_ps(pos, _mm256_cvtepi32_ps(i));
            let a = gather(t.outs[o], i);
            let b = gather(t.outs[o], _mm256_add_epi32(i, step));
            *y = _mm256_add_ps(a, _mm256_mul_ps(_mm256_sub_ps(b, a), f));
        }
        y
    }

    /// `Sample::from_unit(f64::from(y))` for `u8` (`max` 255) or `u16`
    /// (65535): `y * max + 0.5` in `f64`, truncated, saturated (NaN to 0).
    #[inline]
    #[target_feature(enable = "avx2")]
    fn quantise(y: __m256, max: f64) -> __m256i {
        let k = _mm256_set1_pd(max);
        let h = _mm256_set1_pd(0.5);
        let z = _mm256_setzero_pd();
        let lo = _mm256_cvtps_pd(_mm256_castps256_ps128(y));
        let hi = _mm256_cvtps_pd(_mm256_extractf128_ps::<1>(y));
        // `maxpd` takes its second operand (0) for NaN; the saturating cast
        // of a value in 0..=max is its truncation.
        let lo = _mm256_min_pd(_mm256_max_pd(_mm256_add_pd(_mm256_mul_pd(lo, k), h), z), k);
        let hi = _mm256_min_pd(_mm256_max_pd(_mm256_add_pd(_mm256_mul_pd(hi, k), h), z), k);
        _mm256_set_m128i(_mm256_cvttpd_epi32(hi), _mm256_cvttpd_epi32(lo))
    }

    // ---- 8-bit pixels ----

    /// Eight pixels of interleaved 8-bit RGB (`SC` 3) or RGBA (`SC` 4) as
    /// four lanes of 32-bit channel values (alpha 0 for RGB).
    #[inline]
    #[target_feature(enable = "avx2")]
    fn load_u8<const SC: usize>(p: &[u8]) -> [__m256i; 4] {
        if SC == 4 {
            let v = load256(arr::<32>(p));
            let ff = _mm256_set1_epi32(0xff);
            [
                _mm256_and_si256(v, ff),
                _mm256_and_si256(_mm256_srli_epi32::<8>(v), ff),
                _mm256_and_si256(_mm256_srli_epi32::<16>(v), ff),
                _mm256_srli_epi32::<24>(v),
            ]
        } else {
            let p: &[u8; 24] = arr::<24>(p);
            let a = load128(arr::<16>(&p[..16]));
            let b = load64(arr::<8>(&p[16..]));
            let ch = |c: usize| -> __m256i {
                // Safe intrinsics in a closure: the closure inherits the
                // enclosing function's target features.
                _mm256_cvtepu8_epi32(_mm_or_si128(
                    _mm_shuffle_epi8(a, mask(rgb8_in(c, 0))),
                    _mm_shuffle_epi8(b, mask(rgb8_in(c, 1))),
                ))
            };
            [ch(0), ch(1), ch(2), _mm256_setzero_si256()]
        }
    }

    /// Eight pixels of 8-bit codes (each lane 0..=255) to interleaved RGB
    /// (`DC` 3) or RGBA (`DC` 4, alpha from `a`).
    #[inline]
    #[target_feature(enable = "avx2")]
    fn store_u8<const DC: usize>(p: &mut [u8], [r, g, b, a]: [__m256i; 4]) {
        let w = _mm256_or_si256(
            _mm256_or_si256(r, _mm256_slli_epi32::<8>(g)),
            _mm256_slli_epi32::<16>(b),
        );
        if DC == 4 {
            store256(
                arr_mut::<32>(p),
                _mm256_or_si256(w, _mm256_slli_epi32::<24>(a)),
            );
        } else {
            let p: &mut [u8; 24] = arr_mut::<24>(p);
            let m = _mm256_broadcastsi128_si256(mask(RGB8_OUT));
            let packed = _mm256_shuffle_epi8(w, m);
            let lo = _mm256_castsi256_si128(packed);
            let hi = _mm256_extracti128_si256::<1>(packed);
            // Bytes 0..12 from the low half (its store also writes 12..16,
            // which the high half's 12 bytes then overwrite).
            store128(arr_mut::<16>(&mut p[..16]), lo);
            store64(arr_mut::<8>(&mut p[12..20]), hi);
            p[20..24].copy_from_slice(&(_mm_extract_epi32::<2>(hi) as u32).to_le_bytes());
        }
    }

    #[target_feature(enable = "avx2")]
    fn run_u8<const SC: usize, const DC: usize>(
        t: &ShaperTables<'_, 256>,
        codes: Option<&[Codes8<'_>; 3]>,
        src: &[u8],
        dst: &mut [u8],
        copy_alpha: bool,
    ) -> usize {
        let opaque = _mm256_set1_epi32(0xff);
        let mut done = 0;
        let pairs = src.chunks_exact(8 * SC).zip(dst.chunks_exact_mut(8 * DC));
        if let Some(c) = codes {
            for (s, d) in pairs {
                let [r, g, b, a] = load_u8::<SC>(s);
                let v = mix(t, [r, g, b]);
                let a = if copy_alpha { a } else { opaque };
                let q = [
                    codes8(v[0], &c[0]),
                    codes8(v[1], &c[1]),
                    codes8(v[2], &c[2]),
                ];
                store_u8::<DC>(d, [q[0], q[1], q[2], a]);
                done += 8;
            }
            return done;
        }
        for (s, d) in pairs {
            let [r, g, b, a] = load_u8::<SC>(s);
            let y = shade(t, [r, g, b]);
            let a = if copy_alpha { a } else { opaque };
            store_u8::<DC>(
                d,
                [
                    quantise(y[0], 255.0),
                    quantise(y[1], 255.0),
                    quantise(y[2], 255.0),
                    a,
                ],
            );
            done += 8;
        }
        done
    }

    /// The 8-bit kernel.
    ///
    /// # Safety
    /// The CPU must support AVX2.
    #[target_feature(enable = "avx2")]
    pub(super) fn shaper_u8(
        t: &ShaperTables<'_, 256>,
        codes: Option<&[Codes8<'_>; 3]>,
        src: &[u8],
        dst: &mut [u8],
        sc_dc: (usize, usize),
        copy_alpha: bool,
    ) -> usize {
        match sc_dc {
            (3, 3) => run_u8::<3, 3>(t, codes, src, dst, copy_alpha),
            (3, 4) => run_u8::<3, 4>(t, codes, src, dst, copy_alpha),
            (4, 3) => run_u8::<4, 3>(t, codes, src, dst, copy_alpha),
            (4, 4) => run_u8::<4, 4>(t, codes, src, dst, copy_alpha),
            _ => 0,
        }
    }

    // ---- 16-bit pixels ----

    /// Eight pixels of interleaved 16-bit RGB (`SC` 3) or RGBA (`SC` 4) as
    /// four lanes of 32-bit channel values (alpha 0 for RGB).
    #[inline]
    #[target_feature(enable = "avx2")]
    fn load_u16<const SC: usize>(p: &[u16]) -> [__m256i; 4] {
        let block = |k: usize| -> __m128i {
            load_words(
                p[8 * k..]
                    .first_chunk::<8>()
                    .expect("chunk of the kernel's own width"),
            )
        };
        if SC == 4 {
            let v0 = _mm256_set_m128i(block(1), block(0));
            let v1 = _mm256_set_m128i(block(3), block(2));
            let even_odd = _mm256_setr_epi32(0, 2, 4, 6, 1, 3, 5, 7);
            let p0 = _mm256_permutevar8x32_epi32(v0, even_odd);
            let p1 = _mm256_permutevar8x32_epi32(v1, even_odd);
            let rg = _mm256_permute2x128_si256::<0x20>(p0, p1);
            let ba = _mm256_permute2x128_si256::<0x31>(p0, p1);
            let lo = _mm256_set1_epi32(0xffff);
            [
                _mm256_and_si256(rg, lo),
                _mm256_srli_epi32::<16>(rg),
                _mm256_and_si256(ba, lo),
                _mm256_srli_epi32::<16>(ba),
            ]
        } else {
            let blk = [block(0), block(1), block(2)];
            let ch = |c: usize| -> __m256i {
                let mut v = _mm_shuffle_epi8(blk[0], mask(rgb16_in(c, 0)));
                v = _mm_or_si128(v, _mm_shuffle_epi8(blk[1], mask(rgb16_in(c, 1))));
                v = _mm_or_si128(v, _mm_shuffle_epi8(blk[2], mask(rgb16_in(c, 2))));
                _mm256_cvtepu16_epi32(v)
            };
            [ch(0), ch(1), ch(2), _mm256_setzero_si256()]
        }
    }

    /// Eight pixels of 16-bit codes (each lane 0..=65535) to interleaved
    /// RGB (`DC` 3) or RGBA (`DC` 4, alpha from `a`).
    #[inline]
    #[target_feature(enable = "avx2")]
    fn store_u16<const DC: usize>(p: &mut [u16], [r, g, b, a]: [__m256i; 4]) {
        let put = |p: &mut [u16], k: usize, v: __m128i| {
            store_words(
                p[8 * k..]
                    .first_chunk_mut::<8>()
                    .expect("chunk of the kernel's own width"),
                v,
            );
        };
        if DC == 4 {
            let rg = _mm256_or_si256(r, _mm256_slli_epi32::<16>(g));
            let ba = _mm256_or_si256(b, _mm256_slli_epi32::<16>(a));
            let lo = _mm256_unpacklo_epi32(rg, ba);
            let hi = _mm256_unpackhi_epi32(rg, ba);
            let o0 = _mm256_permute2x128_si256::<0x20>(lo, hi);
            let o1 = _mm256_permute2x128_si256::<0x31>(lo, hi);
            put(p, 0, _mm256_castsi256_si128(o0));
            put(p, 1, _mm256_extracti128_si256::<1>(o0));
            put(p, 2, _mm256_castsi256_si128(o1));
            put(p, 3, _mm256_extracti128_si256::<1>(o1));
        } else {
            let pack = |v: __m256i| {
                _mm_packus_epi32(_mm256_castsi256_si128(v), _mm256_extracti128_si256::<1>(v))
            };
            let ch = [pack(r), pack(g), pack(b)];
            for j in 0..3 {
                let mut v = _mm_shuffle_epi8(ch[0], mask(rgb16_out(0, j)));
                v = _mm_or_si128(v, _mm_shuffle_epi8(ch[1], mask(rgb16_out(1, j))));
                v = _mm_or_si128(v, _mm_shuffle_epi8(ch[2], mask(rgb16_out(2, j))));
                put(p, j, v);
            }
        }
    }

    #[target_feature(enable = "avx2")]
    fn run_u16<const SC: usize, const DC: usize>(
        t: &ShaperTables<'_, 65536>,
        src: &[u16],
        dst: &mut [u16],
        copy_alpha: bool,
    ) -> usize {
        let opaque = _mm256_set1_epi32(0xffff);
        let mut done = 0;
        for (s, d) in src.chunks_exact(8 * SC).zip(dst.chunks_exact_mut(8 * DC)) {
            let [r, g, b, a] = load_u16::<SC>(s);
            let y = shade(t, [r, g, b]);
            let a = if copy_alpha { a } else { opaque };
            store_u16::<DC>(
                d,
                [
                    quantise(y[0], 65535.0),
                    quantise(y[1], 65535.0),
                    quantise(y[2], 65535.0),
                    a,
                ],
            );
            done += 8;
        }
        done
    }

    /// The 16-bit kernel.
    ///
    /// # Safety
    /// The CPU must support AVX2.
    #[target_feature(enable = "avx2")]
    pub(super) fn shaper_u16(
        t: &ShaperTables<'_, 65536>,
        src: &[u16],
        dst: &mut [u16],
        sc_dc: (usize, usize),
        copy_alpha: bool,
    ) -> usize {
        match sc_dc {
            (3, 3) => run_u16::<3, 3>(t, src, dst, copy_alpha),
            (3, 4) => run_u16::<3, 4>(t, src, dst, copy_alpha),
            (4, 3) => run_u16::<4, 3>(t, src, dst, copy_alpha),
            (4, 4) => run_u16::<4, 4>(t, src, dst, copy_alpha),
            _ => 0,
        }
    }

    // ---- fast `f32` pixels ----

    /// Eight `f32`s.
    #[inline]
    #[target_feature(enable = "avx2")]
    fn load_ps(p: &[f32; 8]) -> __m256 {
        // SAFETY: `p` refers to 32 readable bytes; the load is unaligned.
        unsafe { _mm256_loadu_ps(p.as_ptr()) }
    }

    #[inline]
    #[target_feature(enable = "avx2")]
    fn store_ps(p: &mut [f32; 8], v: __m256) {
        // SAFETY: `p` refers to 32 writable bytes; the store is unaligned.
        unsafe { _mm256_storeu_ps(p.as_mut_ptr(), v) }
    }

    /// [`crate::fast::interp`] on eight lanes: the cell `i = min(trunc(pos),
    /// CELLS - 1)` (unsigned: a negative index would cap too), `[a, d]` from
    /// it, `a + d * (pos - i)`.
    #[inline]
    #[target_feature(enable = "avx2")]
    fn interp(t: &[[f32; 2]; CELLS], pos: __m256) -> __m256 {
        let i = _mm256_min_epu32(
            _mm256_cvttps_epi32(pos),
            _mm256_set1_epi32(CELLS as i32 - 1),
        );
        let f = _mm256_sub_ps(pos, _mm256_cvtepi32_ps(i));
        // SAFETY: every lane of `i`, read as unsigned, is at most CELLS - 1
        // (the `min` above), so each 8-byte read at `t + 8 * i` is one whole
        // cell inside the CELLS cells `t` refers to.
        let (lo, hi) = unsafe {
            (
                _mm256_i32gather_epi64::<8>(t.as_ptr().cast(), _mm256_castsi256_si128(i)),
                _mm256_i32gather_epi64::<8>(t.as_ptr().cast(), _mm256_extracti128_si256::<1>(i)),
            )
        };
        // Cells (a0 d0 a1 d1 | a2 d2 a3 d3) and (a4 d4 ..): even lanes are
        // `a`, odd ones `d`, back into lane order.
        let (lo, hi) = (_mm256_castsi256_ps(lo), _mm256_castsi256_ps(hi));
        let a = _mm256_castpd_ps(_mm256_permute4x64_pd::<0xD8>(_mm256_castps_pd(
            _mm256_shuffle_ps::<0x88>(lo, hi),
        )));
        let d = _mm256_castpd_ps(_mm256_permute4x64_pd::<0xD8>(_mm256_castps_pd(
            _mm256_shuffle_ps::<0xDD>(lo, hi),
        )));
        _mm256_add_ps(a, _mm256_mul_ps(d, f))
    }

    /// [`FastCurve::forward_at`] on eight lanes.
    #[inline]
    #[target_feature(enable = "avx2")]
    fn forward(f: &FastCurve, x: __m256) -> __m256 {
        let scale = _mm256_set1_ps((FAST_TABLE - 1) as f32);
        let t = interp(&f.table, _mm256_mul_ps(x, scale));
        match f.toe {
            Some((d, k)) => {
                let below = _mm256_cmp_ps::<_CMP_LT_OQ>(x, _mm256_set1_ps(d));
                _mm256_blendv_ps(t, _mm256_mul_ps(x, _mm256_set1_ps(k)), below)
            }
            None => t,
        }
    }

    /// [`FastCurve::inverse_at`] on eight lanes.
    #[inline]
    #[target_feature(enable = "avx2")]
    fn inverse(f: &FastCurve, y: __m256) -> __m256 {
        let scale = _mm256_set1_ps((FAST_TABLE - 1) as f32);
        let t = interp(&f.table, _mm256_mul_ps(_mm256_sqrt_ps(y), scale));
        match f.toe {
            Some((brk, k)) => {
                let below = _mm256_cmp_ps::<_CMP_LT_OQ>(y, _mm256_set1_ps(brk));
                _mm256_blendv_ps(t, _mm256_mul_ps(y, _mm256_set1_ps(k)), below)
            }
            None => t,
        }
    }

    /// Eight pixels of interleaved `f32` RGB (`SC` 3) or RGBA (`SC` 4) as
    /// channel vectors in pixel order (alpha 0 for RGB).
    #[inline]
    #[target_feature(enable = "avx2")]
    fn load_f32<const SC: usize>(p: &[f32]) -> [__m256; 4] {
        let v = |k: usize| load_ps(p[8 * k..].first_chunk::<8>().expect("kernel width"));
        if SC == 4 {
            let (v0, v1, v2, v3) = (v(0), v(1), v(2), v(3));
            let t0 = _mm256_unpacklo_ps(v0, v1);
            let t1 = _mm256_unpackhi_ps(v0, v1);
            let t2 = _mm256_unpacklo_ps(v2, v3);
            let t3 = _mm256_unpackhi_ps(v2, v3);
            // (p0 p2 p4 p6 | p1 p3 p5 p7) back to pixel order.
            let order = _mm256_setr_epi32(0, 4, 1, 5, 2, 6, 3, 7);
            let o = |x: __m256| _mm256_permutevar8x32_ps(x, order);
            [
                o(_mm256_shuffle_ps::<0x44>(t0, t2)),
                o(_mm256_shuffle_ps::<0xEE>(t0, t2)),
                o(_mm256_shuffle_ps::<0x44>(t1, t3)),
                o(_mm256_shuffle_ps::<0xEE>(t1, t3)),
            ]
        } else {
            let (v0, v1, v2) = (v(0), v(1), v(2));
            // Channel c of pixel k is sample 3k + c: blend each sample into
            // place from the vector holding it, then permute into pixel order.
            let r = _mm256_permutevar8x32_ps(
                _mm256_blend_ps::<0b0010_0100>(_mm256_blend_ps::<0b1001_0010>(v0, v1), v2),
                _mm256_setr_epi32(0, 3, 6, 1, 4, 7, 2, 5),
            );
            let g = _mm256_permutevar8x32_ps(
                _mm256_blend_ps::<0b0100_1001>(_mm256_blend_ps::<0b0010_0100>(v0, v1), v2),
                _mm256_setr_epi32(1, 4, 7, 2, 5, 0, 3, 6),
            );
            let b = _mm256_permutevar8x32_ps(
                _mm256_blend_ps::<0b1001_0010>(_mm256_blend_ps::<0b0100_1001>(v0, v1), v2),
                _mm256_setr_epi32(2, 5, 0, 3, 6, 1, 4, 7),
            );
            [r, g, b, _mm256_setzero_ps()]
        }
    }

    /// Eight pixels of channel vectors (pixel order) to interleaved `f32`
    /// RGB (`DC` 3) or RGBA (`DC` 4).
    #[inline]
    #[target_feature(enable = "avx2")]
    fn store_f32<const DC: usize>(p: &mut [f32], [r, g, b, a]: [__m256; 4]) {
        let mut put = |k: usize, v: __m256| {
            store_ps(p[8 * k..].first_chunk_mut::<8>().expect("kernel width"), v);
        };
        if DC == 4 {
            // Pixel order to (p0 p2 p4 p6 | p1 p3 p5 p7), then the transpose.
            let order = _mm256_setr_epi32(0, 2, 4, 6, 1, 3, 5, 7);
            let o = |x: __m256| _mm256_permutevar8x32_ps(x, order);
            let (r, g, b, a) = (o(r), o(g), o(b), o(a));
            let t0 = _mm256_unpacklo_ps(r, g);
            let t1 = _mm256_unpacklo_ps(b, a);
            let t2 = _mm256_unpackhi_ps(r, g);
            let t3 = _mm256_unpackhi_ps(b, a);
            put(0, _mm256_shuffle_ps::<0x44>(t0, t1));
            put(1, _mm256_shuffle_ps::<0xEE>(t0, t1));
            put(2, _mm256_shuffle_ps::<0x44>(t2, t3));
            put(3, _mm256_shuffle_ps::<0xEE>(t2, t3));
        } else {
            let r = _mm256_permutevar8x32_ps(r, _mm256_setr_epi32(0, 3, 6, 1, 4, 7, 2, 5));
            let g = _mm256_permutevar8x32_ps(g, _mm256_setr_epi32(5, 0, 3, 6, 1, 4, 7, 2));
            let b = _mm256_permutevar8x32_ps(b, _mm256_setr_epi32(2, 5, 0, 3, 6, 1, 4, 7));
            put(
                0,
                _mm256_blend_ps::<0b0010_0100>(_mm256_blend_ps::<0b1001_0010>(r, g), b),
            );
            put(
                1,
                _mm256_blend_ps::<0b0010_0100>(_mm256_blend_ps::<0b1001_0010>(b, r), g),
            );
            put(
                2,
                _mm256_blend_ps::<0b0010_0100>(_mm256_blend_ps::<0b1001_0010>(g, b), r),
            );
        }
    }

    #[target_feature(enable = "avx2")]
    fn run_f32<const SC: usize, const DC: usize>(
        t: &FastShaperView<'_>,
        src: &[f32],
        dst: &mut [f32],
        copy_alpha: bool,
        near: &mut Vec<usize>,
    ) -> usize {
        let abs = _mm256_castsi256_ps(_mm256_set1_epi32(0x7FFF_FFFF));
        let guard = _mm256_set1_ps(GUARD);
        let one = _mm256_set1_ps(1.0);
        let mut done = 0;
        for (s, d) in src.chunks_exact(8 * SC).zip(dst.chunks_exact_mut(8 * DC)) {
            let [r, g, b, a] = load_f32::<SC>(s);
            let x = [r, g, b];
            let mut lin = [_mm256_setzero_ps(); 3];
            for c in 0..3 {
                let v = clamp01(x[c]);
                lin[c] = match t.fwd[c] {
                    Some(f) => forward(f, v),
                    None => v,
                };
            }
            let v = product(&t.m, &t.off, lin);
            let mut y = [_mm256_setzero_ps(); 3];
            let mut close = _mm256_setzero_ps();
            for o in 0..3 {
                let v = clamp01(v[o]);
                y[o] = match t.inv[o] {
                    Some(f) => {
                        if let Some((brk, _)) = f.toe {
                            let d = _mm256_and_ps(_mm256_sub_ps(v, _mm256_set1_ps(brk)), abs);
                            close = _mm256_or_ps(close, _mm256_cmp_ps::<_CMP_LT_OQ>(d, guard));
                        }
                        clamp01(inverse(f, v))
                    }
                    None => v,
                };
            }
            let mut bits = _mm256_movemask_ps(close) as u32;
            while bits != 0 {
                near.push(done + bits.trailing_zeros() as usize);
                bits &= bits - 1;
            }
            let a = if copy_alpha { a } else { one };
            store_f32::<DC>(d, [y[0], y[1], y[2], a]);
            done += 8;
        }
        done
    }

    /// The fast `f32` kernel.
    ///
    /// # Safety
    /// The CPU must support AVX2.
    #[target_feature(enable = "avx2")]
    pub(super) fn shaper_f32_fast(
        t: &FastShaperView<'_>,
        src: &[f32],
        dst: &mut [f32],
        sc_dc: (usize, usize),
        copy_alpha: bool,
        near: &mut Vec<usize>,
    ) -> usize {
        match sc_dc {
            (3, 3) => run_f32::<3, 3>(t, src, dst, copy_alpha, near),
            (3, 4) => run_f32::<3, 4>(t, src, dst, copy_alpha, near),
            (4, 3) => run_f32::<4, 3>(t, src, dst, copy_alpha, near),
            (4, 4) => run_f32::<4, 4>(t, src, dst, copy_alpha, near),
            _ => 0,
        }
    }

    // ---- fast `f32` LUT chains ----

    /// `Table1::eval` on eight clamped lanes: the cell `min(trunc(x * scale),
    /// last)` (unsigned), `a + d * (pos - i)`.
    #[inline]
    #[target_feature(enable = "avx2")]
    fn table1(t: &Table1, x: __m256) -> __m256 {
        let pos = _mm256_mul_ps(x, _mm256_set1_ps(t.scale));
        let last = t.last.min(t.cells.len().saturating_sub(1) as u32);
        let i = _mm256_min_epu32(_mm256_cvttps_epi32(pos), _mm256_set1_epi32(last as i32));
        let f = _mm256_sub_ps(pos, _mm256_cvtepi32_ps(i));
        if t.cells.is_empty() {
            return x;
        }
        // SAFETY: `cells` is non-empty (tested above) and every lane of `i`,
        // read as unsigned, is at most `last <= cells.len() - 1` (the `min`
        // above), so each 8-byte read at `cells + 8 * i` is one whole cell.
        let (lo, hi) = unsafe {
            (
                _mm256_i32gather_epi64::<8>(t.cells.as_ptr().cast(), _mm256_castsi256_si128(i)),
                _mm256_i32gather_epi64::<8>(
                    t.cells.as_ptr().cast(),
                    _mm256_extracti128_si256::<1>(i),
                ),
            )
        };
        let (a, d) = split_cells(lo, hi);
        _mm256_add_ps(a, _mm256_mul_ps(d, f))
    }

    /// Two gathers of four `[a, d]` cells back to lane order.
    #[inline]
    #[target_feature(enable = "avx2")]
    fn split_cells(lo: __m256i, hi: __m256i) -> (__m256, __m256) {
        let (lo, hi) = (_mm256_castsi256_ps(lo), _mm256_castsi256_ps(hi));
        let a = _mm256_castpd_ps(_mm256_permute4x64_pd::<0xD8>(_mm256_castps_pd(
            _mm256_shuffle_ps::<0x88>(lo, hi),
        )));
        let d = _mm256_castpd_ps(_mm256_permute4x64_pd::<0xD8>(_mm256_castps_pd(
            _mm256_shuffle_ps::<0xDD>(lo, hi),
        )));
        (a, d)
    }

    /// A stage of tables on clamped lanes (`crate::fast::apply`).
    #[inline]
    #[target_feature(enable = "avx2")]
    fn stage(t: &Option<Table1>, x: __m256) -> __m256 {
        let c = clamp01(x);
        match t {
            Some(t) => table1(t, c),
            None => c,
        }
    }

    /// `LabF::eval` on eight lanes.
    #[inline]
    #[target_feature(enable = "avx2")]
    fn lab_f(l: &LabF, t: __m256) -> __m256 {
        let lin = _mm256_add_ps(
            _mm256_mul_ps(_mm256_set1_ps(841.0 / 108.0), t),
            _mm256_set1_ps(16.0 / 116.0),
        );
        let low = _mm256_cmp_ps::<_CMP_LE_OQ>(t, _mm256_set1_ps(LABF_LIMIT));
        // `if t < TOP { t } else { TOP }`: `minps` takes its second operand
        // unless the first is less.
        let tc = _mm256_min_ps(t, _mm256_set1_ps(LABF_TOP));
        // Lanes at or below the limit take the linear part; their bucket is
        // computed from the limit instead, so every index is in range.
        let tc = _mm256_max_ps(tc, _mm256_set1_ps(LABF_LIMIT));
        let off = _mm256_sub_epi32(_mm256_castps_si256(tc), _mm256_set1_epi32(LABF_BASE as i32));
        let i = _mm256_min_epu32(
            _mm256_srli_epi32::<{ LABF_SHIFT as i32 }>(off),
            _mm256_set1_epi32(LABF_CELLS as i32 - 1),
        );
        let f = _mm256_mul_ps(
            _mm256_cvtepi32_ps(_mm256_and_si256(
                off,
                _mm256_set1_epi32((1 << LABF_SHIFT) - 1),
            )),
            _mm256_set1_ps(1.0 / (1u32 << LABF_SHIFT) as f32),
        );
        // SAFETY: every lane of `i`, read as unsigned, is at most
        // LABF_CELLS - 1 (the `min` above), so each 8-byte read is one whole
        // cell of the LABF_CELLS `cells` refers to.
        let (lo, hi) = unsafe {
            (
                _mm256_i32gather_epi64::<8>(l.cells.as_ptr().cast(), _mm256_castsi256_si128(i)),
                _mm256_i32gather_epi64::<8>(
                    l.cells.as_ptr().cast(),
                    _mm256_extracti128_si256::<1>(i),
                ),
            )
        };
        let (a, d) = split_cells(lo, hi);
        _mm256_blendv_ps(_mm256_add_ps(a, _mm256_mul_ps(d, f)), lin, low)
    }

    /// `crate::fast::lab_f_inv` on eight lanes.
    #[inline]
    #[target_feature(enable = "avx2")]
    fn lab_f_inv(t: __m256) -> __m256 {
        let lin = _mm256_mul_ps(
            _mm256_set1_ps(108.0 / 841.0),
            _mm256_sub_ps(t, _mm256_set1_ps(16.0 / 116.0)),
        );
        let cube = _mm256_mul_ps(_mm256_mul_ps(t, t), t);
        let low = _mm256_cmp_ps::<_CMP_LE_OQ>(t, _mm256_set1_ps(24.0 / 116.0));
        _mm256_blendv_ps(cube, lin, low)
    }

    /// One CLUT point.
    #[inline]
    #[target_feature(enable = "avx2")]
    fn node(p: &[f32; 4]) -> __m128 {
        // SAFETY: `p` refers to 16 readable bytes; the load is unaligned.
        unsafe { _mm_loadu_ps(p.as_ptr()) }
    }

    /// `a + (b - a) * f` lane-wise (`crate::fast::lerp4`), `f` broadcast.
    #[inline]
    #[target_feature(enable = "avx2")]
    fn lerp(a: __m128, b: __m128, f: __m128) -> __m128 {
        _mm_add_ps(a, _mm_mul_ps(_mm_sub_ps(b, a), f))
    }

    /// Lane `j` of an integer vector.
    #[inline]
    #[target_feature(enable = "avx2")]
    fn lane(v: __m256i, j: usize) -> usize {
        _mm256_cvtsi256_si32(_mm256_permutevar8x32_epi32(v, _mm256_set1_epi32(j as i32))) as u32
            as usize
    }

    /// Lane `j` of a float vector, in all four lanes of a 128-bit one.
    #[inline]
    #[target_feature(enable = "avx2")]
    fn bcast(v: __m256, j: usize) -> __m128 {
        _mm256_castps256_ps128(_mm256_permutevar8x32_ps(v, _mm256_set1_epi32(j as i32)))
    }

    /// `FastClut::axis` on eight lanes: (cell, fraction).
    #[inline]
    #[target_feature(enable = "avx2")]
    fn axis(c: &FastClut, k: usize, x: __m256) -> (__m256i, __m256) {
        let p = _mm256_mul_ps(clamp01(x), _mm256_set1_ps((c.g[k] - 1) as f32));
        let i = _mm256_min_epu32(_mm256_cvttps_epi32(p), _mm256_set1_epi32(c.g[k] as i32 - 2));
        (i, _mm256_sub_ps(p, _mm256_cvtepi32_ps(i)))
    }

    /// The tetrahedra of eight cells (`crate::fast::order` per lane): the
    /// `ORDER` code and the fractions from the largest to the smallest.
    #[inline]
    #[target_feature(enable = "avx2")]
    fn sorted(f: [__m256; 3]) -> (__m256i, [__m256; 3]) {
        let ge = |a: __m256, b: __m256| {
            _mm256_srli_epi32::<31>(_mm256_castps_si256(_mm256_cmp_ps::<_CMP_GE_OQ>(a, b)))
        };
        let code = _mm256_or_si256(
            _mm256_or_si256(ge(f[0], f[1]), _mm256_slli_epi32::<1>(ge(f[1], f[2]))),
            _mm256_slli_epi32::<2>(ge(f[0], f[2])),
        );
        let hi = _mm256_max_ps(_mm256_max_ps(f[0], f[1]), f[2]);
        let lo = _mm256_min_ps(_mm256_min_ps(f[0], f[1]), f[2]);
        let mid = _mm256_max_ps(
            _mm256_min_ps(f[0], f[1]),
            _mm256_min_ps(_mm256_max_ps(f[0], f[1]), f[2]),
        );
        (code, [hi, mid, lo])
    }

    /// One tetrahedral lookup (`FastClut::tetra`): corners along the axes in
    /// `ORDER[code]`, weights the sorted fractions (the same values the
    /// scalar picks by axis).
    #[inline]
    #[target_feature(enable = "avx2")]
    fn tetra(c: &FastClut, base: usize, code: usize, s: [usize; 3], w: [__m128; 3]) -> __m128 {
        let [a, b, _] = crate::fast::ORDER[code & 7];
        let p0 = node(&c.nodes[base]);
        let p1 = node(&c.nodes[base + s[a]]);
        let p2 = node(&c.nodes[base + s[a] + s[b]]);
        let p3 = node(&c.nodes[base + s[0] + s[1] + s[2]]);
        let v = _mm_add_ps(p0, _mm_mul_ps(_mm_sub_ps(p1, p0), w[0]));
        let v = _mm_add_ps(v, _mm_mul_ps(_mm_sub_ps(p2, p1), w[1]));
        _mm_add_ps(v, _mm_mul_ps(_mm_sub_ps(p3, p2), w[2]))
    }

    /// Eight four-lane results to four channel vectors in pixel order.
    #[inline]
    #[target_feature(enable = "avx2")]
    fn transpose(r: [__m128; 8]) -> [__m256; 4] {
        let pair = |a: __m128, b: __m128| _mm256_insertf128_ps::<1>(_mm256_castps128_ps256(a), b);
        let (a0, a1, a2, a3) = (
            pair(r[0], r[4]),
            pair(r[1], r[5]),
            pair(r[2], r[6]),
            pair(r[3], r[7]),
        );
        let t0 = _mm256_unpacklo_ps(a0, a1);
        let t1 = _mm256_unpackhi_ps(a0, a1);
        let t2 = _mm256_unpacklo_ps(a2, a3);
        let t3 = _mm256_unpackhi_ps(a2, a3);
        [
            _mm256_shuffle_ps::<0x44>(t0, t2),
            _mm256_shuffle_ps::<0xEE>(t0, t2),
            _mm256_shuffle_ps::<0x44>(t1, t3),
            _mm256_shuffle_ps::<0xEE>(t1, t3),
        ]
    }

    /// Three-input CLUT lookups of eight pixels (`FastClut::lookup3`).
    #[inline]
    #[target_feature(enable = "avx2")]
    fn clut3(c: &FastClut, x: [__m256; 3]) -> [__m256; 4] {
        let (i0, f0) = axis(c, 0, x[0]);
        let (i1, f1) = axis(c, 1, x[1]);
        let (i2, f2) = axis(c, 2, x[2]);
        let s = [c.strides[0], c.strides[1], c.strides[2]];
        let base = _mm256_add_epi32(
            _mm256_add_epi32(
                _mm256_mullo_epi32(i0, _mm256_set1_epi32(s[0] as i32)),
                _mm256_mullo_epi32(i1, _mm256_set1_epi32(s[1] as i32)),
            ),
            _mm256_mullo_epi32(i2, _mm256_set1_epi32(s[2] as i32)),
        );
        let mut r = [_mm_setzero_ps(); 8];
        if c.trilinear {
            let nd = &c.nodes;
            let (a, b, cc) = (s[0], s[1], s[2]);
            for (j, r) in r.iter_mut().enumerate() {
                let o = lane(base, j);
                let (w0, w1, w2) = (bcast(f0, j), bcast(f1, j), bcast(f2, j));
                let l00 = lerp(node(&nd[o]), node(&nd[o + cc]), w2);
                let l01 = lerp(node(&nd[o + b]), node(&nd[o + b + cc]), w2);
                let l10 = lerp(node(&nd[o + a]), node(&nd[o + a + cc]), w2);
                let l11 = lerp(node(&nd[o + a + b]), node(&nd[o + a + b + cc]), w2);
                *r = lerp(lerp(l00, l01, w1), lerp(l10, l11, w1), w0);
            }
        } else {
            let (code, w) = sorted([f0, f1, f2]);
            for (j, r) in r.iter_mut().enumerate() {
                let ww = [bcast(w[0], j), bcast(w[1], j), bcast(w[2], j)];
                *r = tetra(c, lane(base, j), lane(code, j), s, ww);
            }
        }
        transpose(r)
    }

    /// Four-input CLUT lookups of eight pixels (`FastClut::lookup4`).
    #[inline]
    #[target_feature(enable = "avx2")]
    fn clut4(c: &FastClut, x: [__m256; 4]) -> [__m256; 4] {
        let (iw, fw) = axis(c, 0, x[0]);
        let (i1, f1) = axis(c, 1, x[1]);
        let (i2, f2) = axis(c, 2, x[2]);
        let (i3, f3) = axis(c, 3, x[3]);
        let s = [c.strides[1], c.strides[2], c.strides[3]];
        let st = |i: __m256i, s: usize| _mm256_mullo_epi32(i, _mm256_set1_epi32(s as i32));
        let base = _mm256_add_epi32(
            _mm256_add_epi32(st(iw, c.strides[0]), st(i1, s[0])),
            _mm256_add_epi32(st(i2, s[1]), st(i3, s[2])),
        );
        let (code, w) = sorted([f1, f2, f3]);
        let mut r = [_mm_setzero_ps(); 8];
        for (j, r) in r.iter_mut().enumerate() {
            let (o, k) = (lane(base, j), lane(code, j));
            let ww = [bcast(w[0], j), bcast(w[1], j), bcast(w[2], j)];
            let lo = tetra(c, o, k, s, ww);
            let hi = tetra(c, o + c.strides[0], k, s, ww);
            *r = lerp(lo, hi, bcast(fw, j));
        }
        transpose(r)
    }

    /// RGB into a LUT output profile, eight pixels at a time (`FastLutOut`):
    /// curves, map, Lab encoding, input tables, CLUT, output tables, clamp,
    /// all on channel vectors.
    #[target_feature(enable = "avx2")]
    fn run_out<const SC: usize, const DC: usize>(
        o: &FastLutOut,
        src: &[f32],
        dst: &mut [f32],
        copy_alpha: bool,
    ) -> usize {
        let k = _mm256_set1_ps(o.lab_scale);
        let mut done = 0;
        for (s, d) in src.chunks_exact(8 * SC).zip(dst.chunks_exact_mut(8 * DC)) {
            let [r, g, b, a] = load_f32::<SC>(s);
            let x = [r, g, b];
            let mut lin = [_mm256_setzero_ps(); 3];
            for c in 0..3 {
                lin[c] = forward(&o.fwd[c], clamp01(x[c]));
            }
            let t = product(&o.m, &o.off, lin);
            let fx = lab_f(&o.lab, t[0]);
            let fy = lab_f(&o.lab, t[1]);
            let fz = lab_f(&o.lab, t[2]);
            let l = _mm256_sub_ps(
                _mm256_mul_ps(_mm256_set1_ps(116.0), fy),
                _mm256_set1_ps(16.0),
            );
            let aa = _mm256_mul_ps(_mm256_set1_ps(500.0), _mm256_sub_ps(fx, fy));
            let bb = _mm256_mul_ps(_mm256_set1_ps(200.0), _mm256_sub_ps(fy, fz));
            let inv255 = _mm256_set1_ps(1.0 / 255.0);
            let c128 = _mm256_set1_ps(128.0);
            let enc = [
                clamp01(_mm256_mul_ps(l, _mm256_set1_ps(0.01))),
                clamp01(_mm256_mul_ps(_mm256_add_ps(aa, c128), inv255)),
                clamp01(_mm256_mul_ps(_mm256_add_ps(bb, c128), inv255)),
            ];
            let mut e = [_mm256_setzero_ps(); 3];
            for c in 0..3 {
                e[c] = stage(&o.pre[c], _mm256_mul_ps(enc[c], k));
            }
            let v = clut3(&o.clut, e);
            let mut y = [_mm256_setzero_ps(); 4];
            for c in 0..4 {
                y[c] = clamp01(stage(&o.post[c], v[c]));
            }
            if DC == 4 && o.outputs == 4 {
                store_f32::<4>(d, y);
            } else if DC == 3 {
                store_f32::<3>(d, y);
            } else {
                // Three outputs and alpha.
                let al = if copy_alpha { a } else { _mm256_set1_ps(1.0) };
                store_f32::<4>(d, [y[0], y[1], y[2], al]);
            }
            done += 8;
        }
        done
    }

    /// A four-input LUT input profile into RGB, eight pixels at a time
    /// (`FastLutIn`), all on channel vectors.
    #[target_feature(enable = "avx2")]
    fn run_in<const DC: usize>(i: &FastLutIn, src: &[f32], dst: &mut [f32]) -> usize {
        let k = _mm256_set1_ps(i.lab_scale);
        let mut done = 0;
        for (s, d) in src.chunks_exact(32).zip(dst.chunks_exact_mut(8 * DC)) {
            let x = load_f32::<4>(s);
            let mut e = [_mm256_setzero_ps(); 4];
            for c in 0..4 {
                e[c] = stage(&i.pre[c], x[c]);
            }
            let v = clut4(&i.clut, e);
            let p = [0, 1, 2].map(|c| _mm256_mul_ps(stage(&i.post[c], v[c]), k));
            let l = _mm256_mul_ps(p[0], _mm256_set1_ps(100.0));
            let c128 = _mm256_set1_ps(128.0);
            let c255 = _mm256_set1_ps(255.0);
            let a = _mm256_sub_ps(_mm256_mul_ps(p[1], c255), c128);
            let b = _mm256_sub_ps(_mm256_mul_ps(p[2], c255), c128);
            let fy = _mm256_mul_ps(
                _mm256_add_ps(l, _mm256_set1_ps(16.0)),
                _mm256_set1_ps(1.0 / 116.0),
            );
            let t = [
                lab_f_inv(_mm256_add_ps(fy, _mm256_mul_ps(_mm256_set1_ps(0.002), a))),
                lab_f_inv(fy),
                lab_f_inv(_mm256_sub_ps(fy, _mm256_mul_ps(_mm256_set1_ps(0.005), b))),
            ];
            let lin = product(&i.m, &i.off, t);
            let y = [0, 1, 2].map(|c| clamp01(inverse(&i.inv[c], clamp01(lin[c]))));
            if DC == 3 {
                store_f32::<3>(d, [y[0], y[1], y[2], _mm256_setzero_ps()]);
            } else {
                store_f32::<4>(d, [y[0], y[1], y[2], _mm256_set1_ps(1.0)]);
            }
            done += 8;
        }
        done
    }

    /// The fast `f32` LUT-chain kernel.
    ///
    /// # Safety
    /// The CPU must support AVX2.
    #[target_feature(enable = "avx2")]
    pub(super) fn chain_f32_fast(
        f: &FastChain,
        src: &[f32],
        dst: &mut [f32],
        sc_dc: (usize, usize),
        copy_alpha: bool,
    ) -> usize {
        // Shapes the vector stores cover: four outputs into CMYK, three
        // into RGB or RGBA; CMYK in (no alpha) into RGB or RGBA. The rest
        // (CMYKA either side) take the scalar twin.
        match (f, sc_dc) {
            (FastChain::Out(o), (3, 3)) if o.outputs == 3 => {
                run_out::<3, 3>(o, src, dst, copy_alpha)
            }
            (FastChain::Out(o), (3, 4)) => run_out::<3, 4>(o, src, dst, copy_alpha),
            (FastChain::Out(o), (4, 3)) if o.outputs == 3 => {
                run_out::<4, 3>(o, src, dst, copy_alpha)
            }
            (FastChain::Out(o), (4, 4)) => run_out::<4, 4>(o, src, dst, copy_alpha),
            (FastChain::In(i, 4), (4, 3)) => run_in::<3>(i, src, dst),
            (FastChain::In(i, 4), (4, 4)) => run_in::<4>(i, src, dst),
            _ => 0,
        }
    }

    // ---- integer device-link grid: three inputs, four outputs ----

    /// A grid of four `f32`s per point, at least one point long (what
    /// [`point`] relies on), `g` points per axis, first input slowest.
    pub(super) struct Cube<'a> {
        data: &'a [f32],
        /// `data.len() - 4`: the last offset a point can start at.
        top: usize,
        g: usize,
        /// Strides in `f32`s.
        s: [usize; 3],
    }

    impl<'a> Cube<'a> {
        /// `None` when `data` has fewer than four values.
        pub(super) fn new(data: &'a [f32], g: usize) -> Option<Self> {
            Some(Self {
                data,
                top: data.len().checked_sub(4)?,
                g,
                s: [4 * g * g, 4 * g, 4],
            })
        }
    }

    /// The grid point at `off` (in `f32`s), clamped to the last point, as
    /// four `f64`s (exact: every `f32` is an `f64`).
    #[inline]
    #[target_feature(enable = "avx2")]
    fn point(c: &Cube<'_>, off: usize) -> __m256d {
        let off = off.min(c.top);
        // SAFETY: `c.top` is `c.data.len() - 4` (`Cube::new` declines a
        // shorter `data`, and nothing else sets the field) and `off` is at
        // most `c.top` (the line above), so the 16 bytes read at
        // `data + off` lie inside `data`; the load is unaligned.
        unsafe { _mm256_cvtps_pd(_mm_loadu_ps(c.data.as_ptr().add(off))) }
    }

    /// `ClutEval::tetra_select::<4>` at three `(cell, fraction)`s: the same
    /// corners, the same differences, products and sums in the same order
    /// (no FMA), the four outputs in the four lanes. Cells below `g` read
    /// the corners the scalar reads (the walk's four, all inside a grid of
    /// `g^3` points); the clamp in [`point`] only bounds the reads.
    ///
    /// The corner offsets are formed in vector registers, lane per axis:
    /// written as scalar masks, LLVM's cmov conversion turned some
    /// instances of them back into the branches `tetra_select` exists to
    /// remove (the 8-bit kernel ran 1.5x slower than the scalar loop).
    #[inline]
    #[target_feature(enable = "avx2")]
    fn tetra34(c: &Cube<'_>, [(c0, rx), (c1, ry), (c2, rz)]: [(usize, f64); 3]) -> __m256d {
        let [s0, s1, s2] = c.s;
        let origin = c0 * s0 + c1 * s1 + c2 * s2;
        // Lanes x, y, z, (none). Steps: the stride where `cell + 1 < g`,
        // i.e. `g - 1 > cell` (cells and strides are far below 2^63).
        let strides = _mm256_set_epi64x(0, s2 as i64, s1 as i64, s0 as i64);
        let cells = _mm256_set_epi64x(0, c2 as i64, c1 as i64, c0 as i64);
        let top = _mm256_set1_epi64x(c.g as i64 - 1);
        let steps = _mm256_and_si256(_mm256_cmpgt_epi64(top, cells), strides);
        // [rx >= ry, rx >= rz, ry >= rz, true]: x before y, x before z,
        // y before z (the walk's order, ties to the earlier axis).
        let f = _mm256_set_pd(0.0, rz, ry, rx);
        let m = _mm256_castpd_si256(_mm256_cmp_pd::<_CMP_GE_OQ>(
            _mm256_permute4x64_pd::<0b11_01_00_00>(f),
            _mm256_permute4x64_pd::<0b11_10_10_01>(f),
        ));
        // The corner before each axis step: the steps of the axes ahead.
        // x: y if y before x (!xy), z if z before x (!xz);
        // y: x if xy, z if !yz; z: x if xz, y if yz.
        let first = _mm256_and_si256(
            _mm256_xor_si256(
                _mm256_permute4x64_epi64::<0b11_01_00_00>(m),
                _mm256_set_epi64x(0, 0, 0, -1),
            ),
            _mm256_permute4x64_epi64::<0b11_00_00_01>(steps),
        );
        let second = _mm256_and_si256(
            _mm256_xor_si256(
                _mm256_permute4x64_epi64::<0b11_10_10_01>(m),
                _mm256_set_epi64x(0, 0, -1, -1),
            ),
            _mm256_permute4x64_epi64::<0b11_01_10_10>(steps),
        );
        // [before x, before y, before z, origin] and the corners after.
        let before = _mm256_add_epi64(
            _mm256_add_epi64(_mm256_set1_epi64x(origin as i64), first),
            second,
        );
        let after = _mm256_add_epi64(before, steps);
        let off = |v: __m128i| {
            (
                _mm_cvtsi128_si64(v) as usize,
                _mm_extract_epi64::<1>(v) as usize,
            )
        };
        let (bx, by) = off(_mm256_castsi256_si128(before));
        let (bz, _) = off(_mm256_extracti128_si256::<1>(before));
        let (ax, ay) = off(_mm256_castsi256_si128(after));
        let (az, _) = off(_mm256_extracti128_si256::<1>(after));
        let dx = _mm256_sub_pd(point(c, ax), point(c, bx));
        let dy = _mm256_sub_pd(point(c, ay), point(c, by));
        let dz = _mm256_sub_pd(point(c, az), point(c, bz));
        let acc = _mm256_add_pd(point(c, origin), _mm256_mul_pd(dx, _mm256_set1_pd(rx)));
        let acc = _mm256_add_pd(acc, _mm256_mul_pd(dy, _mm256_set1_pd(ry)));
        _mm256_add_pd(acc, _mm256_mul_pd(dz, _mm256_set1_pd(rz)))
    }

    /// `Sample::from_unit` on four lanes: `(v * scale + 0.5)` truncated
    /// with the saturating cast's ends (`max` with 0 first, which also maps
    /// NaN to 0 as the cast does: `maxpd` returns its second operand then;
    /// then `min` with `scale`), as `i32`s.
    #[inline]
    #[target_feature(enable = "avx2")]
    fn quantise_pd(v: __m256d, scale: f64) -> __m128i {
        let s = _mm256_set1_pd(scale);
        let q = _mm256_add_pd(_mm256_mul_pd(v, s), _mm256_set1_pd(0.5));
        _mm256_cvttpd_epi32(_mm256_min_pd(_mm256_max_pd(q, _mm256_setzero_pd()), s))
    }

    /// The 16-bit grid kernel (`SC` samples per source pixel, the first
    /// three looked up).
    #[inline]
    #[target_feature(enable = "avx2")]
    fn run34_u16<T: Copy, C: Fn(T) -> (usize, f64), const SC: usize>(
        c: &Cube<'_>,
        cell: &C,
        src: &[T],
        dst: &mut [u16],
    ) -> usize {
        let mut done = 0;
        for (sp, dp) in src.chunks_exact(SC).zip(dst.chunks_exact_mut(4)) {
            let v = tetra34(c, [cell(sp[0]), cell(sp[1]), cell(sp[2])]);
            let w = _mm_packus_epi32(quantise_pd(v, 65535.0), _mm_setzero_si128());
            let b = _mm_cvtsi128_si64(w) as u64;
            dp.copy_from_slice(&[
                b as u16,
                (b >> 16) as u16,
                (b >> 32) as u16,
                (b >> 48) as u16,
            ]);
            done += 1;
        }
        done
    }

    /// The 8-bit grid kernel.
    #[inline]
    #[target_feature(enable = "avx2")]
    fn run34_u8<T: Copy, C: Fn(T) -> (usize, f64), const SC: usize>(
        c: &Cube<'_>,
        cell: &C,
        src: &[T],
        dst: &mut [u8],
    ) -> usize {
        let mut done = 0;
        for (sp, dp) in src.chunks_exact(SC).zip(dst.chunks_exact_mut(4)) {
            let v = tetra34(c, [cell(sp[0]), cell(sp[1]), cell(sp[2])]);
            let w = _mm_packus_epi32(quantise_pd(v, 255.0), _mm_setzero_si128());
            let b = _mm_cvtsi128_si32(_mm_packus_epi16(w, w)) as u32;
            dp.copy_from_slice(&b.to_le_bytes());
            done += 1;
        }
        done
    }

    /// The four lanes of a vector, in order.
    #[cfg(test)]
    #[inline]
    #[target_feature(enable = "avx2")]
    fn lanes_pd(v: __m256d) -> [f64; 4] {
        let (lo, hi) = (_mm256_castpd256_pd128(v), _mm256_extractf128_pd::<1>(v));
        [
            _mm_cvtsd_f64(lo),
            _mm_cvtsd_f64(_mm_unpackhi_pd(lo, lo)),
            _mm_cvtsd_f64(hi),
            _mm_cvtsd_f64(_mm_unpackhi_pd(hi, hi)),
        ]
    }

    #[cfg(test)]
    #[inline]
    #[target_feature(enable = "avx2")]
    fn lanes_epi32(v: __m128i) -> [i32; 4] {
        [
            _mm_extract_epi32::<0>(v),
            _mm_extract_epi32::<1>(v),
            _mm_extract_epi32::<2>(v),
            _mm_extract_epi32::<3>(v),
        ]
    }

    /// [`super::grid34_parts`].
    ///
    /// # Safety
    /// The CPU must support AVX2.
    #[cfg(test)]
    #[target_feature(enable = "avx2")]
    pub(super) fn grid34_parts(
        c: &Cube<'_>,
        cells: [(usize, f64); 3],
        q: [f64; 4],
    ) -> ([f64; 4], [i32; 4], [i32; 4]) {
        let q = _mm256_set_pd(q[3], q[2], q[1], q[0]);
        (
            lanes_pd(tetra34(c, cells)),
            lanes_epi32(quantise_pd(q, 65535.0)),
            lanes_epi32(quantise_pd(q, 255.0)),
        )
    }

    /// The 16-bit three-in, four-out grid kernel.
    ///
    /// # Safety
    /// The CPU must support AVX2.
    #[target_feature(enable = "avx2")]
    pub(super) fn grid34_u16<T: Copy, C: Fn(T) -> (usize, f64)>(
        c: &Cube<'_>,
        cell: &C,
        src: &[T],
        sc: usize,
        dst: &mut [u16],
    ) -> usize {
        match sc {
            3 => run34_u16::<T, C, 3>(c, cell, src, dst),
            4 => run34_u16::<T, C, 4>(c, cell, src, dst),
            _ => 0,
        }
    }

    /// The 8-bit three-in, four-out grid kernel.
    ///
    /// # Safety
    /// The CPU must support AVX2.
    #[target_feature(enable = "avx2")]
    pub(super) fn grid34_u8<T: Copy, C: Fn(T) -> (usize, f64)>(
        c: &Cube<'_>,
        cell: &C,
        src: &[T],
        sc: usize,
        dst: &mut [u8],
    ) -> usize {
        match sc {
            3 => run34_u8::<T, C, 3>(c, cell, src, dst),
            4 => run34_u8::<T, C, 4>(c, cell, src, dst),
            _ => 0,
        }
    }
}
