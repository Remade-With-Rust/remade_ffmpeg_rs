//! SIMD twins of the hot scalar kernels.
//!
//! This is the only module allowed `unsafe` (the crate is otherwise
//! `deny(unsafe_code)`). Every kernel here has a scalar twin elsewhere in
//! the crate that stays the oracle and the fallback: the wrappers below
//! take fixed-size arrays or check slice lengths before entering any
//! intrinsic code, pick the kernel from a CPU feature check made once per
//! process, and return the scalar result's exact bits (the tests compare
//! every twin with its oracle bit for bit).
//!
//! x86-64 only: SSE2 (the x86-64 baseline, so no detection) and AVX2 with
//! F16C (detected at run time). Every other architecture runs the scalar
//! code. The float kernels perform the scalar code's operations in the same
//! order with the same operands, with no FMA (Rust never contracts a
//! multiply and an add, and these kernels do not either), so their results
//! are bit-identical to the scalar ones, not merely close; they are only
//! used on finite data, where IEEE addition and multiplication are
//! commutative bit for bit (NaN operand order is the one thing that could
//! differ, and the callers keep non-finite data on the scalar path).

#![allow(unsafe_code)]

/// Which kernel family the process uses.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Isa {
    /// The scalar code (non-x86-64, or forced).
    Scalar,
    /// SSE2 kernels (every x86-64 CPU).
    #[cfg_attr(not(target_arch = "x86_64"), allow(dead_code))]
    Sse2,
    /// AVX2 + F16C kernels.
    #[cfg_attr(not(target_arch = "x86_64"), allow(dead_code))]
    Avx2,
}

/// The kernel family for this CPU, detected once. The environment variable
/// `RUSTY_EXR_ISA` (`scalar`, `sse2`) can force a lower family; it exists
/// for A/B measurements and never selects a family the CPU lacks.
pub(crate) fn isa() -> Isa {
    static ISA: std::sync::OnceLock<Isa> = std::sync::OnceLock::new();
    *ISA.get_or_init(detect)
}

fn detect() -> Isa {
    let forced = std::env::var("RUSTY_EXR_ISA").ok();
    match forced.as_deref() {
        Some("scalar") => return Isa::Scalar,
        Some("sse2") => return lower(Isa::Sse2),
        _ => {}
    }
    best()
}

#[cfg(target_arch = "x86_64")]
fn best() -> Isa {
    if std::arch::is_x86_feature_detected!("avx2") && std::arch::is_x86_feature_detected!("f16c") {
        Isa::Avx2
    } else {
        Isa::Sse2
    }
}

#[cfg(not(target_arch = "x86_64"))]
fn best() -> Isa {
    Isa::Scalar
}

fn lower(want: Isa) -> Isa {
    match (want, best()) {
        (_, Isa::Scalar) => Isa::Scalar,
        (Isa::Sse2, _) => Isa::Sse2,
        (w, _) => w,
    }
}

/// Every ISA this CPU can run (for the oracle tests).
#[cfg(test)]
pub(crate) fn available() -> Vec<Isa> {
    match best() {
        Isa::Avx2 => vec![Isa::Sse2, Isa::Avx2],
        Isa::Sse2 => vec![Isa::Sse2],
        Isa::Scalar => vec![],
    }
}

// ---------------------------------------------------------------------------
// DWA decode: one block (1 component, or an R/G/B triple).

/// The SIMD twin of [`crate::compression::dwa::block_scalar`] for a block
/// that is not flat in every component. Returns `false`, writing nothing,
/// when a coefficient is infinite or NaN (the scalar path then handles the
/// block) or when no SIMD family is in use.
pub(crate) fn dwa_block(
    isa: Isa,
    raster: &[[u16; 64]; 3],
    zr: [u8; 3],
    n: usize,
    out: &mut [[u16; 64]; 3],
) -> bool {
    assert!((1..=3).contains(&n) && zr.iter().all(|&z| z <= 8));
    match isa {
        #[cfg(target_arch = "x86_64")]
        // SAFETY: SSE2 is part of x86-64; the arrays are fixed-size and the
        // kernel indexes only within them (n <= 3, zr <= 8 checked above).
        Isa::Sse2 => unsafe { x86::dwa_block_sse2(raster, zr, n, out) },
        #[cfg(target_arch = "x86_64")]
        // SAFETY: `Isa::Avx2` is only selected after detecting AVX2 and F16C.
        Isa::Avx2 => unsafe { x86::dwa_block_avx2(raster, zr, n, out) },
        _ => false,
    }
}

// ---------------------------------------------------------------------------
// PIZ: level 1 of the inverse wavelet over one row pair.

/// Inverse-wavelet level 1 over the first `len` words of a row pair:
/// `len` covers whole 2x2 groups of `unit`-word samples (`unit` 1 for
/// HALF planes, 2 for the two interleaved word planes of FLOAT/UINT), each
/// group being samples `x` and `x + unit` of `top` and `bot`. Processes a
/// prefix of whole 16-word chunks and returns its length; the caller
/// finishes the rest with the scalar code. Bit-identical to `piz`'s
/// `wdec14`/`wdec16` group step (wrapping 16-bit integer arithmetic).
pub(crate) fn piz_wdec_rows(
    isa: Isa,
    top: &mut [u16],
    bot: &mut [u16],
    len: usize,
    unit: usize,
    w14: bool,
) -> usize {
    assert!(len <= top.len() && len <= bot.len() && (unit == 1 || unit == 2));
    let chunks = len / 16;
    match isa {
        #[cfg(target_arch = "x86_64")]
        // SAFETY: SSE2 is part of x86-64; the kernel touches words
        // `0..chunks * 16`, inside both slices (checked above).
        Isa::Sse2 => unsafe { x86::wdec_rows_sse2(top, bot, chunks, unit, w14) },
        #[cfg(target_arch = "x86_64")]
        // SAFETY: as above, and AVX2 was detected before `Isa::Avx2` is chosen.
        Isa::Avx2 => unsafe { x86::wdec_rows_avx2(top, bot, chunks, unit, w14) },
        _ => return 0,
    }
    chunks * 16
}

// ---------------------------------------------------------------------------
// Prefix sums: the ZIP/RLE/DWA byte predictor and PXR24's row deltas.

/// Undo the ZIP predictor (`t[i] = t[i-1] + t[i] - 128`, mod 256) over a
/// prefix of `t`: byte 0 plus whole 16-byte steps. Returns the prefix
/// length (0 when no SIMD family is in use); the caller finishes with
/// `zip::undo_predictor_from`.
pub(crate) fn undo_predictor(isa: Isa, t: &mut [u8]) -> usize {
    if t.len() < 17 {
        return 0;
    }
    let steps = (t.len() - 1) / 16;
    match isa {
        #[cfg(target_arch = "x86_64")]
        // SAFETY: SSE2 is part of x86-64; bytes 0..1 + 16 * steps are in `t`.
        Isa::Sse2 | Isa::Avx2 => unsafe { x86::undo_predictor_sse2(t, steps) },
        _ => return 0,
    }
    1 + 16 * steps
}

/// PXR24 HALF row: `out` (little-endian u16) is the running sum of the
/// big-endian deltas whose high bytes are `hi` and low bytes `lo` (the
/// twin of `pxr24::row_16`). Returns `false` when no SIMD family is in use.
pub(crate) fn pxr24_row_16(isa: Isa, hi: &[u8], lo: &[u8], out: &mut [u8]) -> bool {
    let w = hi.len();
    assert!(lo.len() == w && out.len() == 2 * w);
    match isa {
        #[cfg(target_arch = "x86_64")]
        // SAFETY: SSE2 is part of x86-64; lengths checked above.
        Isa::Sse2 | Isa::Avx2 => unsafe { x86::pxr24_row_16_sse2(hi, lo, out) },
        _ => return false,
    }
    true
}

/// PXR24 FLOAT/UINT row: the running sum of big-endian 32-bit deltas whose
/// bytes, most significant first, are `p[0]`, `p[1]`, `p[2]` and `p3` (zero
/// for FLOAT's 24-bit samples). The twin of `pxr24::row_32`.
pub(crate) fn pxr24_row_32(isa: Isa, p: [&[u8]; 3], p3: Option<&[u8]>, out: &mut [u8]) -> bool {
    let w = p[0].len();
    assert!(p.iter().all(|q| q.len() == w) && p3.is_none_or(|q| q.len() == w));
    assert!(out.len() == 4 * w);
    match isa {
        #[cfg(target_arch = "x86_64")]
        // SAFETY: SSE2 is part of x86-64; lengths checked above.
        Isa::Sse2 | Isa::Avx2 => unsafe { x86::pxr24_row_32_sse2(p, p3, out) },
        _ => return false,
    }
    true
}

#[cfg(target_arch = "x86_64")]
#[allow(clippy::wildcard_imports)]
mod x86 {
    use std::arch::x86_64::*;

    use crate::compression::dwa::{A, B, C, D, E, F, G, ZR_CONSTANT};

    // OpenEXR's AVX row-pass matrices (see `dwa::row_pass`), one row each.
    const M1: [[f32; 4]; 4] = [[A, A, A, A], [C, F, -F, -C], [A, -A, -A, A], [F, -C, C, -F]];
    const M2: [[f32; 4]; 4] = [[B, D, E, G], [D, -G, -B, -E], [E, -B, G, D], [G, -E, D, -B]];

    /// True when any of the 64 halves is infinite or NaN.
    #[inline(always)]
    unsafe fn any_nonfinite(h: &[u16; 64]) -> bool {
        let m = _mm_set1_epi16(0x7c00);
        let mut acc = _mm_setzero_si128();
        for k in 0..8 {
            let v = _mm_loadu_si128(h.as_ptr().add(k * 8).cast());
            acc = _mm_or_si128(acc, _mm_cmpeq_epi16(_mm_and_si128(v, m), m));
        }
        _mm_movemask_epi8(acc) != 0
    }

    /// `crate::half::f16_to_f32` on four halves (zero-extended to u32 lanes),
    /// operation for operation.
    #[inline(always)]
    unsafe fn h2f4(x: __m128i) -> __m128 {
        let shifted = _mm_slli_epi32(_mm_and_si128(x, _mm_set1_epi32(0x7fff)), 13);
        let exp = _mm_and_si128(shifted, _mm_set1_epi32(0x0f80_0000));
        let mut o = _mm_add_epi32(shifted, _mm_set1_epi32((127 - 15) << 23));
        let infnan = _mm_cmpeq_epi32(exp, _mm_set1_epi32(0x0f80_0000));
        o = _mm_add_epi32(o, _mm_and_si128(infnan, _mm_set1_epi32((128 - 16) << 23)));
        let man_zero =
            _mm_cmpeq_epi32(_mm_and_si128(x, _mm_set1_epi32(0x3ff)), _mm_setzero_si128());
        let quiet = _mm_andnot_si128(man_zero, infnan);
        o = _mm_or_si128(o, _mm_and_si128(quiet, _mm_set1_epi32(0x0040_0000)));
        let den = _mm_cmpeq_epi32(exp, _mm_setzero_si128());
        let od = _mm_castps_si128(_mm_sub_ps(
            _mm_castsi128_ps(_mm_add_epi32(o, _mm_set1_epi32(1 << 23))),
            _mm_castsi128_ps(_mm_set1_epi32(113 << 23)),
        ));
        o = _mm_or_si128(_mm_and_si128(den, od), _mm_andnot_si128(den, o));
        let sign = _mm_slli_epi32(_mm_and_si128(x, _mm_set1_epi32(0x8000)), 16);
        _mm_castsi128_ps(_mm_or_si128(o, sign))
    }

    /// `crate::half::f32_to_f16` on four floats; the result is in the low
    /// 16 bits of each u32 lane (sign-extended to 32 bits, ready for
    /// `_mm_packs_epi32`).
    #[inline(always)]
    unsafe fn f2h4(f: __m128) -> __m128i {
        const INF: i32 = 255 << 23;
        const HALF_OVER: i32 = (127 + 16) << 23;
        const DENORM_MAGIC: i32 = ((127 - 15) + (23 - 10) + 1) << 23;
        let x = _mm_castps_si128(f);
        let sign = _mm_and_si128(_mm_srli_epi32(x, 16), _mm_set1_epi32(0x8000));
        let a = _mm_and_si128(x, _mm_set1_epi32(0x7fff_ffff));
        // a >= HALF_OVER: infinity, or NaN with its payload.
        let big = _mm_cmpgt_epi32(a, _mm_set1_epi32(HALF_OVER - 1));
        let nan = _mm_cmpgt_epi32(a, _mm_set1_epi32(INF));
        let nan_v = _mm_or_si128(
            _mm_set1_epi32(0x7e00),
            _mm_srli_epi32(_mm_and_si128(a, _mm_set1_epi32(0x007f_ffff)), 13),
        );
        let big_v = _mm_or_si128(
            _mm_and_si128(nan, nan_v),
            _mm_andnot_si128(nan, _mm_set1_epi32(0x7c00)),
        );
        // a < 2^-14: one float addition rounds into the subnormal range.
        let small = _mm_cmplt_epi32(a, _mm_set1_epi32(113 << 23));
        let small_v = _mm_sub_epi32(
            _mm_castps_si128(_mm_add_ps(
                _mm_castsi128_ps(a),
                _mm_castsi128_ps(_mm_set1_epi32(DENORM_MAGIC)),
            )),
            _mm_set1_epi32(DENORM_MAGIC),
        );
        // Normal: round to nearest even by adding 0xfff plus the odd bit.
        let odd = _mm_and_si128(_mm_srli_epi32(a, 13), _mm_set1_epi32(1));
        let norm_v = _mm_srli_epi32(
            _mm_add_epi32(
                _mm_add_epi32(a, _mm_set1_epi32(((15i32 - 127) << 23).wrapping_add(0xfff))),
                odd,
            ),
            13,
        );
        let lo = _mm_or_si128(
            _mm_and_si128(small, small_v),
            _mm_andnot_si128(small, norm_v),
        );
        let o = _mm_or_si128(_mm_and_si128(big, big_v), _mm_andnot_si128(big, lo));
        // Keep the low 16 bits, sign-extended so a signed pack is exact.
        let r = _mm_or_si128(sign, _mm_and_si128(o, _mm_set1_epi32(0xffff)));
        _mm_srai_epi32(_mm_slli_epi32(r, 16), 16)
    }

    /// 64 halves to floats (SSE2).
    #[inline(always)]
    unsafe fn h2f64_sse2(h: &[u16; 64], d: &mut [f32; 64]) {
        let z = _mm_setzero_si128();
        for k in 0..8 {
            let v = _mm_loadu_si128(h.as_ptr().add(k * 8).cast());
            _mm_storeu_ps(d.as_mut_ptr().add(k * 8), h2f4(_mm_unpacklo_epi16(v, z)));
            _mm_storeu_ps(
                d.as_mut_ptr().add(k * 8 + 4),
                h2f4(_mm_unpackhi_epi16(v, z)),
            );
        }
    }

    /// 64 floats to halves (SSE2).
    #[inline(always)]
    unsafe fn f2h64_sse2(d: &[f32; 64], h: &mut [u16; 64]) {
        for k in 0..8 {
            let a = f2h4(_mm_loadu_ps(d.as_ptr().add(k * 8)));
            let b = f2h4(_mm_loadu_ps(d.as_ptr().add(k * 8 + 4)));
            _mm_storeu_si128(h.as_mut_ptr().add(k * 8).cast(), _mm_packs_epi32(a, b));
        }
    }

    /// One row of the AVX row pass (`dwa::row_pass` for finite data):
    /// outputs 0..4 and 4..8.
    #[inline(always)]
    unsafe fn row_pass(r: *const f32) -> (__m128, __m128) {
        let lo = _mm_loadu_ps(r);
        let hi = _mm_loadu_ps(r.add(4));
        let m = |k: usize| _mm_loadu_ps(M1[k].as_ptr());
        let n = |k: usize| _mm_loadu_ps(M2[k].as_ptr());
        let e = _mm_add_ps(
            _mm_add_ps(
                _mm_mul_ps(_mm_shuffle_ps::<0x00>(lo, lo), m(0)),
                _mm_mul_ps(_mm_shuffle_ps::<0xaa>(lo, lo), m(1)),
            ),
            _mm_add_ps(
                _mm_mul_ps(_mm_shuffle_ps::<0x00>(hi, hi), m(2)),
                _mm_mul_ps(_mm_shuffle_ps::<0xaa>(hi, hi), m(3)),
            ),
        );
        let o = _mm_add_ps(
            _mm_add_ps(
                _mm_mul_ps(_mm_shuffle_ps::<0x55>(lo, lo), n(0)),
                _mm_mul_ps(_mm_shuffle_ps::<0xff>(lo, lo), n(1)),
            ),
            _mm_add_ps(
                _mm_mul_ps(_mm_shuffle_ps::<0x55>(hi, hi), n(2)),
                _mm_mul_ps(_mm_shuffle_ps::<0xff>(hi, hi), n(3)),
            ),
        );
        let s = _mm_add_ps(e, o);
        let d = _mm_sub_ps(e, o);
        (s, _mm_shuffle_ps::<0x1b>(d, d))
    }

    /// The column pass of `dwa::idct_avx_impl` over a vector of columns,
    /// with the same per-term presence tests. `$r` holds the row-pass rows.
    macro_rules! col_pass {
        ($r:expr, $present:expr, $add:ident, $sub:ident, $mul:ident, $set1:ident) => {{
            let r = $r;
            let p = |i: usize| i < $present;
            let k = |c: f32| $set1(c);
            let mut o0 = $mul(k(B), r[1]);
            if p(3) {
                o0 = $add($mul(k(D), r[3]), o0);
                if p(5) {
                    let mut t = $mul(k(E), r[5]);
                    if p(7) {
                        t = $add($mul(k(G), r[7]), t);
                    }
                    o0 = $add(t, o0);
                }
            }
            let mut o1 = $mul(k(D), r[1]);
            if p(3) {
                let mut t = $mul(k(G), r[3]);
                if p(5) {
                    t = $add($mul(k(B), r[5]), t);
                }
                o1 = $sub(o1, t);
            }
            if p(7) {
                o1 = $sub(o1, $mul(k(E), r[7]));
            }
            let mut o2 = $mul(k(E), r[1]);
            if p(3) {
                o2 = $sub(o2, $mul(k(B), r[3]));
            }
            if p(5) {
                o2 = $add(o2, $mul(k(G), r[5]));
            }
            if p(7) {
                o2 = $add(o2, $mul(k(D), r[7]));
            }
            let mut o3 = $mul(k(G), r[1]);
            if p(5) {
                o3 = $add($mul(k(D), r[5]), o3);
            }
            if p(3) {
                let mut t = $mul(k(E), r[3]);
                if p(7) {
                    t = $add($mul(k(B), r[7]), t);
                }
                o3 = $sub(o3, t);
            }
            let mut t11 = $mul(k(A), r[0]);
            let mut t1 = t11;
            if p(4) {
                let a4 = $mul(k(A), r[4]);
                t11 = $add(t11, a4);
                t1 = $sub(t1, a4);
            }
            let (mut e0, mut e3, mut e1, mut e2) = (t11, t11, t1, t1);
            if p(2) {
                let mut t7 = $mul(k(C), r[2]);
                if p(6) {
                    t7 = $add(t7, $mul(k(F), r[6]));
                }
                e0 = $add(t11, t7);
                e3 = $sub(t11, t7);
                let mut t7 = $mul(k(F), r[2]);
                if p(6) {
                    t7 = $sub(t7, $mul(k(C), r[6]));
                }
                e1 = $add(t1, t7);
                e2 = $sub(t1, t7);
            }
            [
                $add(e0, o0),
                $add(e1, o1),
                $add(e2, o2),
                $add(e3, o3),
                $sub(e3, o3),
                $sub(e2, o2),
                $sub(e1, o1),
                $sub(e0, o0),
            ]
        }};
    }

    /// `dwa::idct_avx_impl::<false>` (finite data), SSE2.
    #[inline(always)]
    unsafe fn idct_sse2(d: &mut [f32; 64], zr: usize) {
        let present = 8 - zr;
        let mut lo = [_mm_setzero_ps(); 8];
        let mut hi = [_mm_setzero_ps(); 8];
        for i in 0..present {
            (lo[i], hi[i]) = row_pass(d.as_ptr().add(i * 8));
        }
        let p = d.as_mut_ptr();
        if zr == 7 {
            let (a0, a1) = (
                _mm_mul_ps(lo[0], _mm_set1_ps(A)),
                _mm_mul_ps(hi[0], _mm_set1_ps(A)),
            );
            for i in 0..8 {
                _mm_storeu_ps(p.add(i * 8), a0);
                _mm_storeu_ps(p.add(i * 8 + 4), a1);
            }
            return;
        }
        let ol = col_pass!(lo, present, _mm_add_ps, _mm_sub_ps, _mm_mul_ps, _mm_set1_ps);
        let oh = col_pass!(hi, present, _mm_add_ps, _mm_sub_ps, _mm_mul_ps, _mm_set1_ps);
        for i in 0..8 {
            _mm_storeu_ps(p.add(i * 8), ol[i]);
            _mm_storeu_ps(p.add(i * 8 + 4), oh[i]);
        }
    }

    /// The inverse colour transform of `block_scalar` (finite data).
    #[inline(always)]
    unsafe fn csc_sse2(d: &mut [[f32; 64]; 3]) {
        let (c0, c1, c2, c3) = (
            _mm_set1_ps(1.5747),
            _mm_set1_ps(0.1873),
            _mm_set1_ps(0.4682),
            _mm_set1_ps(1.8556),
        );
        for k in 0..16 {
            let s0 = _mm_loadu_ps(d[0].as_ptr().add(k * 4));
            let s1 = _mm_loadu_ps(d[1].as_ptr().add(k * 4));
            let s2 = _mm_loadu_ps(d[2].as_ptr().add(k * 4));
            let r = _mm_add_ps(s0, _mm_mul_ps(c0, s2));
            let g = _mm_sub_ps(_mm_sub_ps(s0, _mm_mul_ps(c1, s1)), _mm_mul_ps(c2, s2));
            let b = _mm_add_ps(s0, _mm_mul_ps(c3, s1));
            _mm_storeu_ps(d[0].as_mut_ptr().add(k * 4), r);
            _mm_storeu_ps(d[1].as_mut_ptr().add(k * 4), g);
            _mm_storeu_ps(d[2].as_mut_ptr().add(k * 4), b);
        }
    }

    /// The flat-component fill of `block_scalar`.
    #[inline(always)]
    fn flat(z0: u16) -> [f32; 64] {
        [crate::half::f16_to_f32(z0) * A * A; 64]
    }

    pub(super) unsafe fn dwa_block_sse2(
        raster: &[[u16; 64]; 3],
        zr: [u8; 3],
        n: usize,
        out: &mut [[u16; 64]; 3],
    ) -> bool {
        if raster[..n].iter().any(|z| any_nonfinite(z)) {
            return false;
        }
        let mut d = [[0f32; 64]; 3];
        for c in 0..n {
            if zr[c] == ZR_CONSTANT {
                d[c] = flat(raster[c][0]);
            } else {
                h2f64_sse2(&raster[c], &mut d[c]);
                idct_sse2(&mut d[c], usize::from(zr[c]));
            }
        }
        if n == 3 {
            csc_sse2(&mut d);
        }
        for c in 0..n {
            f2h64_sse2(&d[c], &mut out[c]);
        }
        true
    }

    // --- AVX2 + F16C ------------------------------------------------------

    #[inline]
    #[target_feature(enable = "avx2,f16c")]
    unsafe fn h2f64_f16c(h: &[u16; 64], d: &mut [f32; 64]) {
        for k in 0..8 {
            let v = _mm_loadu_si128(h.as_ptr().add(k * 8).cast());
            _mm256_storeu_ps(d.as_mut_ptr().add(k * 8), _mm256_cvtph_ps(v));
        }
    }

    #[inline]
    #[target_feature(enable = "avx2,f16c")]
    unsafe fn f2h64_f16c(d: &[f32; 64], h: &mut [u16; 64]) {
        for k in 0..8 {
            let v = _mm256_loadu_ps(d.as_ptr().add(k * 8));
            let r = _mm256_cvtps_ph::<_MM_FROUND_TO_NEAREST_INT>(v);
            _mm_storeu_si128(h.as_mut_ptr().add(k * 8).cast(), r);
        }
    }

    #[inline]
    #[target_feature(enable = "avx2,f16c")]
    unsafe fn idct_avx2(d: &mut [f32; 64], zr: usize) {
        let present = 8 - zr;
        let mut rows = [_mm256_setzero_ps(); 8];
        for i in 0..present {
            let (lo, hi) = row_pass(d.as_ptr().add(i * 8));
            rows[i] = _mm256_set_m128(hi, lo);
        }
        let p = d.as_mut_ptr();
        if zr == 7 {
            let a = _mm256_mul_ps(rows[0], _mm256_set1_ps(A));
            for i in 0..8 {
                _mm256_storeu_ps(p.add(i * 8), a);
            }
            return;
        }
        let o = col_pass!(
            rows,
            present,
            _mm256_add_ps,
            _mm256_sub_ps,
            _mm256_mul_ps,
            _mm256_set1_ps
        );
        for i in 0..8 {
            _mm256_storeu_ps(p.add(i * 8), o[i]);
        }
    }

    #[inline]
    #[target_feature(enable = "avx2,f16c")]
    unsafe fn csc_avx2(d: &mut [[f32; 64]; 3]) {
        let (c0, c1, c2, c3) = (
            _mm256_set1_ps(1.5747),
            _mm256_set1_ps(0.1873),
            _mm256_set1_ps(0.4682),
            _mm256_set1_ps(1.8556),
        );
        for k in 0..8 {
            let s0 = _mm256_loadu_ps(d[0].as_ptr().add(k * 8));
            let s1 = _mm256_loadu_ps(d[1].as_ptr().add(k * 8));
            let s2 = _mm256_loadu_ps(d[2].as_ptr().add(k * 8));
            let r = _mm256_add_ps(s0, _mm256_mul_ps(c0, s2));
            let g = _mm256_sub_ps(
                _mm256_sub_ps(s0, _mm256_mul_ps(c1, s1)),
                _mm256_mul_ps(c2, s2),
            );
            let b = _mm256_add_ps(s0, _mm256_mul_ps(c3, s1));
            _mm256_storeu_ps(d[0].as_mut_ptr().add(k * 8), r);
            _mm256_storeu_ps(d[1].as_mut_ptr().add(k * 8), g);
            _mm256_storeu_ps(d[2].as_mut_ptr().add(k * 8), b);
        }
    }

    #[target_feature(enable = "avx2,f16c")]
    pub(super) unsafe fn dwa_block_avx2(
        raster: &[[u16; 64]; 3],
        zr: [u8; 3],
        n: usize,
        out: &mut [[u16; 64]; 3],
    ) -> bool {
        if raster[..n].iter().any(|z| any_nonfinite(z)) {
            return false;
        }
        let mut d = [[0f32; 64]; 3];
        for c in 0..n {
            if zr[c] == ZR_CONSTANT {
                d[c] = flat(raster[c][0]);
            } else {
                h2f64_f16c(&raster[c], &mut d[c]);
                idct_avx2(&mut d[c], usize::from(zr[c]));
            }
        }
        if n == 3 {
            csc_avx2(&mut d);
        }
        for c in 0..n {
            f2h64_f16c(&d[c], &mut out[c]);
        }
        true
    }

    // --- PIZ inverse wavelet, level 1 -----------------------------------------

    /// Generates the 2x2 group step for one vector width. `$de` splits two
    /// loaded vectors into first and second samples of each pair, `$il`
    /// interleaves them back.
    macro_rules! wdec_kernel {
        ($name:ident, $feat:literal, $V:ty, $n:literal, $load:ident, $store:ident,
         $add:ident, $sub:ident, $and:ident, $srai:ident, $srli:ident, $set1:ident,
         $de1:ident, $il1:ident, $de2:ident, $il2:ident) => {
            #[target_feature(enable = $feat)]
            unsafe fn $name<const W14: bool, const UNIT: usize>(
                top: *mut u16,
                bot: *mut u16,
                chunks: usize,
            ) {
                #[inline]
                #[target_feature(enable = $feat)]
                unsafe fn dec<const W14: bool>(l: $V, h: $V) -> ($V, $V) {
                    if W14 {
                        let a = $add($add(l, $and(h, $set1(1))), $srai::<1>(h));
                        (a, $sub(a, h))
                    } else {
                        let bb = $sub(l, $srli::<1>(h));
                        ($sub($add(h, bb), $set1(-0x8000)), bb)
                    }
                }
                for c in 0..chunks {
                    let (t, b) = (top.add(c * $n), bot.add(c * $n));
                    let lanes = $n / 2;
                    let (t0, t1) = ($load(t.cast()), $load(t.add(lanes).cast()));
                    let (b0, b1) = ($load(b.cast()), $load(b.add(lanes).cast()));
                    let ((ta, tb), (ba, bb)) = if UNIT == 1 {
                        ($de1(t0, t1), $de1(b0, b1))
                    } else {
                        ($de2(t0, t1), $de2(b0, b1))
                    };
                    let (i00, i10) = dec::<W14>(ta, ba);
                    let (i01, i11) = dec::<W14>(tb, bb);
                    let (a, bv) = dec::<W14>(i00, i01);
                    let (cv, dv) = dec::<W14>(i10, i11);
                    let ((o0, o1), (o2, o3)) = if UNIT == 1 {
                        ($il1(a, bv), $il1(cv, dv))
                    } else {
                        ($il2(a, bv), $il2(cv, dv))
                    };
                    $store(t.cast(), o0);
                    $store(t.add(lanes).cast(), o1);
                    $store(b.cast(), o2);
                    $store(b.add(lanes).cast(), o3);
                }
            }
        };
    }

    // 16-bit units: first samples are the even words, second the odd ones.
    #[inline(always)]
    unsafe fn de1_128(v0: __m128i, v1: __m128i) -> (__m128i, __m128i) {
        (
            _mm_packs_epi32(
                _mm_srai_epi32::<16>(_mm_slli_epi32::<16>(v0)),
                _mm_srai_epi32::<16>(_mm_slli_epi32::<16>(v1)),
            ),
            _mm_packs_epi32(_mm_srai_epi32::<16>(v0), _mm_srai_epi32::<16>(v1)),
        )
    }
    #[inline(always)]
    unsafe fn il1_128(a: __m128i, b: __m128i) -> (__m128i, __m128i) {
        (_mm_unpacklo_epi16(a, b), _mm_unpackhi_epi16(a, b))
    }
    // 32-bit units: first samples are the even dwords, second the odd ones.
    #[inline(always)]
    unsafe fn de2_128(v0: __m128i, v1: __m128i) -> (__m128i, __m128i) {
        let s0 = _mm_shuffle_epi32::<0b11_01_10_00>(v0);
        let s1 = _mm_shuffle_epi32::<0b11_01_10_00>(v1);
        (_mm_unpacklo_epi64(s0, s1), _mm_unpackhi_epi64(s0, s1))
    }
    #[inline(always)]
    unsafe fn il2_128(a: __m128i, b: __m128i) -> (__m128i, __m128i) {
        (_mm_unpacklo_epi32(a, b), _mm_unpackhi_epi32(a, b))
    }
    // 256-bit versions: every step stays inside 128-bit lanes, and the
    // interleave undoes the split lane by lane.
    #[inline]
    #[target_feature(enable = "avx2")]
    unsafe fn de1_256(v0: __m256i, v1: __m256i) -> (__m256i, __m256i) {
        (
            _mm256_packs_epi32(
                _mm256_srai_epi32::<16>(_mm256_slli_epi32::<16>(v0)),
                _mm256_srai_epi32::<16>(_mm256_slli_epi32::<16>(v1)),
            ),
            _mm256_packs_epi32(_mm256_srai_epi32::<16>(v0), _mm256_srai_epi32::<16>(v1)),
        )
    }
    #[inline]
    #[target_feature(enable = "avx2")]
    unsafe fn il1_256(a: __m256i, b: __m256i) -> (__m256i, __m256i) {
        (_mm256_unpacklo_epi16(a, b), _mm256_unpackhi_epi16(a, b))
    }
    #[inline]
    #[target_feature(enable = "avx2")]
    unsafe fn de2_256(v0: __m256i, v1: __m256i) -> (__m256i, __m256i) {
        let s0 = _mm256_shuffle_epi32::<0b11_01_10_00>(v0);
        let s1 = _mm256_shuffle_epi32::<0b11_01_10_00>(v1);
        (_mm256_unpacklo_epi64(s0, s1), _mm256_unpackhi_epi64(s0, s1))
    }
    #[inline]
    #[target_feature(enable = "avx2")]
    unsafe fn il2_256(a: __m256i, b: __m256i) -> (__m256i, __m256i) {
        (_mm256_unpacklo_epi32(a, b), _mm256_unpackhi_epi32(a, b))
    }

    wdec_kernel!(
        wdec_sse2,
        "sse2",
        __m128i,
        16,
        _mm_loadu_si128,
        _mm_storeu_si128,
        _mm_add_epi16,
        _mm_sub_epi16,
        _mm_and_si128,
        _mm_srai_epi16,
        _mm_srli_epi16,
        _mm_set1_epi16,
        de1_128,
        il1_128,
        de2_128,
        il2_128
    );
    wdec_kernel!(
        wdec_avx2,
        "avx2",
        __m256i,
        32,
        _mm256_loadu_si256,
        _mm256_storeu_si256,
        _mm256_add_epi16,
        _mm256_sub_epi16,
        _mm256_and_si256,
        _mm256_srai_epi16,
        _mm256_srli_epi16,
        _mm256_set1_epi16,
        de1_256,
        il1_256,
        de2_256,
        il2_256
    );

    pub(super) unsafe fn wdec_rows_sse2(
        top: &mut [u16],
        bot: &mut [u16],
        chunks: usize,
        unit: usize,
        w14: bool,
    ) {
        let (t, b) = (top.as_mut_ptr(), bot.as_mut_ptr());
        match (w14, unit) {
            (true, 1) => wdec_sse2::<true, 1>(t, b, chunks),
            (true, _) => wdec_sse2::<true, 2>(t, b, chunks),
            (false, 1) => wdec_sse2::<false, 1>(t, b, chunks),
            (false, _) => wdec_sse2::<false, 2>(t, b, chunks),
        }
    }

    #[target_feature(enable = "avx2")]
    pub(super) unsafe fn wdec_rows_avx2(
        top: &mut [u16],
        bot: &mut [u16],
        chunks: usize,
        unit: usize,
        w14: bool,
    ) {
        // 32-word chunks, then at most one 16-word chunk.
        let (t, b) = (top.as_mut_ptr(), bot.as_mut_ptr());
        let wide = chunks / 2;
        match (w14, unit) {
            (true, 1) => wdec_avx2::<true, 1>(t, b, wide),
            (true, _) => wdec_avx2::<true, 2>(t, b, wide),
            (false, 1) => wdec_avx2::<false, 1>(t, b, wide),
            (false, _) => wdec_avx2::<false, 2>(t, b, wide),
        }
        if chunks % 2 == 1 {
            let (t, b) = (t.add(wide * 32), b.add(wide * 32));
            match (w14, unit) {
                (true, 1) => wdec_sse2::<true, 1>(t, b, 1),
                (true, _) => wdec_sse2::<true, 2>(t, b, 1),
                (false, 1) => wdec_sse2::<false, 1>(t, b, 1),
                (false, _) => wdec_sse2::<false, 2>(t, b, 1),
            }
        }
    }

    // --- prefix sums ---------------------------------------------------------

    /// `zip::undo_predictor` over byte 0 and `steps` 16-byte steps: each
    /// step is a 4-round log prefix sum of `d - 128` plus the previous byte.
    pub(super) unsafe fn undo_predictor_sse2(t: &mut [u8], steps: usize) {
        let p = t.as_mut_ptr();
        let bias = _mm_set1_epi8(-128);
        let mut prev = _mm_set1_epi8(t[0] as i8);
        for s in 0..steps {
            let q = p.add(1 + 16 * s);
            let mut x = _mm_xor_si128(_mm_loadu_si128(q.cast()), bias);
            x = _mm_add_epi8(x, _mm_slli_si128::<1>(x));
            x = _mm_add_epi8(x, _mm_slli_si128::<2>(x));
            x = _mm_add_epi8(x, _mm_slli_si128::<4>(x));
            x = _mm_add_epi8(x, _mm_slli_si128::<8>(x));
            let out = _mm_add_epi8(x, prev);
            _mm_storeu_si128(q.cast(), out);
            // Broadcast byte 15.
            let hi = _mm_unpackhi_epi8(out, out);
            prev = _mm_shuffle_epi32::<0xff>(_mm_shufflehi_epi16::<0xff>(hi));
        }
    }

    /// `pxr24::row_16`: eight samples per step.
    pub(super) unsafe fn pxr24_row_16_sse2(hi: &[u8], lo: &[u8], out: &mut [u8]) {
        let w = hi.len();
        let steps = w / 8;
        let mut carry = _mm_setzero_si128();
        for s in 0..steps {
            let h = _mm_loadl_epi64(hi.as_ptr().add(8 * s).cast());
            let l = _mm_loadl_epi64(lo.as_ptr().add(8 * s).cast());
            let mut v = _mm_unpacklo_epi8(l, h);
            v = _mm_add_epi16(v, _mm_slli_si128::<2>(v));
            v = _mm_add_epi16(v, _mm_slli_si128::<4>(v));
            v = _mm_add_epi16(v, _mm_slli_si128::<8>(v));
            v = _mm_add_epi16(v, carry);
            _mm_storeu_si128(out.as_mut_ptr().add(16 * s).cast(), v);
            carry = _mm_shuffle_epi32::<0xff>(_mm_shufflehi_epi16::<0xff>(v));
        }
        let mut acc = if steps > 0 {
            u16::from_le_bytes([out[16 * steps - 2], out[16 * steps - 1]])
        } else {
            0
        };
        for x in 8 * steps..w {
            acc = acc.wrapping_add(u16::from_be_bytes([hi[x], lo[x]]));
            out[2 * x..2 * x + 2].copy_from_slice(&acc.to_le_bytes());
        }
    }

    /// `pxr24::row_32`: eight samples per step.
    pub(super) unsafe fn pxr24_row_32_sse2(p: [&[u8]; 3], p3: Option<&[u8]>, out: &mut [u8]) {
        let w = p[0].len();
        let steps = w / 8;
        let z = _mm_setzero_si128();
        let mut carry = z;
        let ld = |q: &[u8], s: usize| _mm_loadl_epi64(q.as_ptr().add(8 * s).cast());
        for s in 0..steps {
            let low = match p3 {
                Some(q) => ld(q, s),
                None => z,
            };
            // Words: (byte 1 | byte 0 << 8) and (byte 3 | byte 2 << 8).
            let lo16 = _mm_unpacklo_epi8(low, ld(p[2], s));
            let hi16 = _mm_unpacklo_epi8(ld(p[1], s), ld(p[0], s));
            for (k, mut v) in [
                _mm_unpacklo_epi16(lo16, hi16),
                _mm_unpackhi_epi16(lo16, hi16),
            ]
            .into_iter()
            .enumerate()
            {
                v = _mm_add_epi32(v, _mm_slli_si128::<4>(v));
                v = _mm_add_epi32(v, _mm_slli_si128::<8>(v));
                v = _mm_add_epi32(v, carry);
                _mm_storeu_si128(out.as_mut_ptr().add(32 * s + 16 * k).cast(), v);
                carry = _mm_shuffle_epi32::<0xff>(v);
            }
        }
        let mut acc = if steps > 0 {
            let o = 32 * steps;
            u32::from_le_bytes([out[o - 4], out[o - 3], out[o - 2], out[o - 1]])
        } else {
            0
        };
        for x in 8 * steps..w {
            let low = p3.map_or(0, |q| q[x]);
            acc = acc.wrapping_add(u32::from_be_bytes([p[0][x], p[1][x], p[2][x], low]));
            out[4 * x..4 * x + 4].copy_from_slice(&acc.to_le_bytes());
        }
    }

    // --- test hooks -------------------------------------------------------

    #[cfg(test)]
    pub(super) fn h2f_sse2_one(h: u16) -> f32 {
        // SAFETY: SSE2 is baseline on x86-64.
        unsafe {
            let v = h2f4(_mm_set1_epi32(i32::from(h)));
            _mm_cvtss_f32(v)
        }
    }

    #[cfg(test)]
    pub(super) fn f2h_sse2_4(f: [f32; 4]) -> [u16; 4] {
        // SAFETY: SSE2 is baseline on x86-64.
        unsafe {
            let r = f2h4(_mm_loadu_ps(f.as_ptr()));
            let p = _mm_packs_epi32(r, r);
            let mut o = [0u16; 8];
            _mm_storeu_si128(o.as_mut_ptr().cast(), p);
            [o[0], o[1], o[2], o[3]]
        }
    }

    #[cfg(test)]
    #[target_feature(enable = "avx2,f16c")]
    pub(super) unsafe fn f2h_f16c_8(f: [f32; 8]) -> [u16; 8] {
        let r = _mm256_cvtps_ph::<_MM_FROUND_TO_NEAREST_INT>(_mm256_loadu_ps(f.as_ptr()));
        let mut o = [0u16; 8];
        _mm_storeu_si128(o.as_mut_ptr().cast(), r);
        o
    }

    #[cfg(test)]
    #[target_feature(enable = "avx2,f16c")]
    pub(super) unsafe fn h2f_f16c_one(h: u16) -> f32 {
        _mm_cvtss_f32(_mm_cvtph_ps(_mm_set1_epi16(h as i16)))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::compression::dwa::{block_scalar, ZR_CONSTANT};
    use crate::half::{f16_to_f32, f32_to_f16};

    struct Rng(u64);
    impl Rng {
        fn next(&mut self) -> u64 {
            self.0 ^= self.0 << 13;
            self.0 ^= self.0 >> 7;
            self.0 ^= self.0 << 17;
            self.0
        }
    }

    #[cfg(target_arch = "x86_64")]
    #[test]
    fn half_to_float_twins_match_every_half() {
        let f16c = best() == Isa::Avx2;
        for h in 0..=u16::MAX {
            let want = f16_to_f32(h).to_bits();
            assert_eq!(x86::h2f_sse2_one(h).to_bits(), want, "{h:04x}");
            if f16c {
                // SAFETY: AVX2 + F16C detected.
                assert_eq!(unsafe { x86::h2f_f16c_one(h) }.to_bits(), want, "{h:04x}");
            }
        }
    }

    #[cfg(target_arch = "x86_64")]
    #[test]
    fn float_to_half_twins_match_scalar() {
        // Every pattern in release builds; a dense stride in debug ones.
        let step: usize = if cfg!(debug_assertions) { 977 } else { 1 };
        let f16c = best() == Isa::Avx2;
        let mut x = 0u64;
        while u32::try_from(x).is_ok() {
            let f: [f32; 8] =
                std::array::from_fn(|k| f32::from_bits((x as u32).wrapping_add(k as u32)));
            let a = x86::f2h_sse2_4([f[0], f[1], f[2], f[3]]);
            for k in 0..4 {
                assert_eq!(a[k], f32_to_f16(f[k]), "{:08x}", f[k].to_bits());
            }
            if f16c {
                // SAFETY: AVX2 + F16C detected.
                let b = unsafe { x86::f2h_f16c_8(f) };
                for k in 0..8 {
                    assert_eq!(b[k], f32_to_f16(f[k]), "{:08x}", f[k].to_bits());
                }
            }
            x += (step * 8) as u64;
        }
    }

    /// Random DWA blocks: every zero-row count, flat components, mixed
    /// magnitudes (cancellation, subnormal halves, signed zeros), triples.
    fn random_block(r: &mut Rng, n: usize) -> ([[u16; 64]; 3], [u8; 3]) {
        let mut raster = [[0u16; 64]; 3];
        let mut zr = [ZR_CONSTANT; 3];
        for c in 0..n {
            let kind = r.next() % 10;
            let z = (r.next() % 9) as u8; // 0..=8
            zr[c] = z;
            let rows = if z == ZR_CONSTANT {
                0
            } else {
                8 - usize::from(z)
            };
            let pick = |r: &mut Rng| -> u16 {
                match kind {
                    0 => (r.next() as u16) & 0x83ff,     // subnormals, signed zeros
                    1 => 0x8000 * (r.next() as u16 & 1), // only zeros
                    2 => (r.next() as u16 % 0x7c00) | (0x8000 * (r.next() as u16 & 1)), // any finite
                    3 => [0x7bff, 0xfbff, 0x3c00, 0xbc00][(r.next() % 4) as usize],     // extremes
                    _ => {
                        let e = 8 + (r.next() % 14) as u16;
                        (e << 10) | (r.next() as u16 & 0x3ff) | (0x8000 * (r.next() as u16 & 1))
                    }
                }
            };
            raster[c][0] = pick(r);
            for i in 1..rows * 8 {
                if r.next() % 3 != 0 {
                    raster[c][i] = pick(r);
                }
            }
            if z != ZR_CONSTANT && raster[c][1].trailing_zeros() >= 15 {
                // Make sure the block is not accidentally flat.
                raster[c][1] = 0x3c00;
            }
        }
        (raster, zr)
    }

    #[test]
    fn dwa_block_twins_match_scalar() {
        let mut r = Rng(0x9e37_79b9_7f4a_7c15);
        let isas = available();
        let rounds = if cfg!(debug_assertions) {
            20_000
        } else {
            400_000
        };
        let mut checked = 0;
        for t in 0..rounds {
            let n = [1, 3, 3, 2][t % 4];
            let (raster, zr) = random_block(&mut r, n);
            let constant = zr[..n].iter().all(|&z| z == ZR_CONSTANT);
            let mut want = [[0u16; 64]; 3];
            block_scalar(&raster, zr, n, constant, &mut want);
            if constant {
                continue;
            }
            for &isa in &isas {
                let mut got = [[0u16; 64]; 3];
                assert!(dwa_block(isa, &raster, zr, n, &mut got));
                assert_eq!(
                    got[..n],
                    want[..n],
                    "{isa:?} block {t} zr {zr:?} {raster:?}"
                );
                checked += 1;
            }
        }
        assert!(isas.is_empty() || checked > 0);
    }

    /// Structural check: every single basis coefficient, at every zero-row
    /// count that keeps it, with values spanning the half range, so each
    /// term of the transform is exercised alone (a reassociated term changes
    /// such outputs where random data might average it away).
    #[test]
    fn dwa_block_twins_match_scalar_per_coefficient() {
        let isas = available();
        let vals = [
            0x0001u16, 0x03ff, 0x0400, 0x3555, 0x3c00, 0x4e3f, 0x7bff, 0x8001, 0xbc00, 0xfbff,
        ];
        for pos in 0..64 {
            for z in 0..8u8 {
                if pos / 8 >= 8 - usize::from(z) {
                    continue;
                }
                for &v in &vals {
                    for &dc in &[0u16, 0x3c00, 0xc500] {
                        let mut raster = [[0u16; 64]; 3];
                        raster[0][0] = dc;
                        raster[0][pos] = v;
                        let zr = [z, ZR_CONSTANT, ZR_CONSTANT];
                        let mut want = [[0u16; 64]; 3];
                        block_scalar(&raster, zr, 1, false, &mut want);
                        for &isa in &isas {
                            let mut got = [[0u16; 64]; 3];
                            assert!(dwa_block(isa, &raster, zr, 1, &mut got));
                            assert_eq!(got[0], want[0], "{isa:?} pos {pos} zr {z} v {v:04x}");
                        }
                    }
                }
            }
        }
    }

    #[test]
    fn dwa_block_declines_non_finite() {
        for &isa in &available() {
            for bad in [0x7c00u16, 0xfc00, 0x7e00, 0x7c01] {
                let mut raster = [[0u16; 64]; 3];
                raster[1][9] = bad;
                raster[0][3] = 0x3c00;
                let mut got = [[0u16; 64]; 3];
                assert!(!dwa_block(isa, &raster, [0, 0, 0], 3, &mut got));
                assert_eq!(got, [[0u16; 64]; 3]);
            }
        }
        assert!(!dwa_block(
            Isa::Scalar,
            &[[0; 64]; 3],
            [0; 3],
            1,
            &mut [[0; 64]; 3]
        ));
    }
}
