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

// Off x86-64 every dispatcher reduces to its scalar arm, so the kernel
// arguments go unused and the code after the early return is unreachable.
#![cfg_attr(not(target_arch = "x86_64"), allow(unused_variables, unreachable_code))]
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
/// that is not flat in every component. `work` is caller-owned float
/// scratch (any contents: every element read is written first), so no
/// block pays for zeroing it. Returns `false`, writing nothing,
/// when a coefficient is infinite or NaN (the scalar path then handles the
/// block) or when no SIMD family is in use.
pub(crate) fn dwa_block(
    isa: Isa,
    raster: &[[u16; 64]; 3],
    zr: [u8; 3],
    n: usize,
    out: &mut [[u16; 64]; 3],
    work: &mut [[f32; 64]; 3],
) -> bool {
    assert!((1..=3).contains(&n) && zr.iter().all(|&z| z <= 8));
    match isa {
        #[cfg(target_arch = "x86_64")]
        // SAFETY: SSE2 is part of x86-64; the arrays are fixed-size and the
        // kernel indexes only within them (n <= 3, zr <= 8 checked above).
        Isa::Sse2 => unsafe { x86::dwa_block_sse2(raster, zr, n, out, work) },
        #[cfg(target_arch = "x86_64")]
        // SAFETY: `Isa::Avx2` is only selected after detecting AVX2 and F16C.
        Isa::Avx2 => unsafe { x86::dwa_block_avx2(raster, zr, n, out, work) },
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
    piz_wav_rows(isa, top, bot, len, unit, w14, false)
}

/// The forward twin of [`piz_wdec_rows`]: `piz`'s `wenc14`/`wenc16` group
/// step of forward level 1 (each pair across, then each column down).
pub(crate) fn piz_wenc_rows(
    isa: Isa,
    top: &mut [u16],
    bot: &mut [u16],
    len: usize,
    unit: usize,
    w14: bool,
) -> usize {
    piz_wav_rows(isa, top, bot, len, unit, w14, true)
}

fn piz_wav_rows(
    isa: Isa,
    top: &mut [u16],
    bot: &mut [u16],
    len: usize,
    unit: usize,
    w14: bool,
    enc: bool,
) -> usize {
    assert!(len <= top.len() && len <= bot.len() && (unit == 1 || unit == 2));
    let chunks = len / 16;
    match isa {
        #[cfg(target_arch = "x86_64")]
        // SAFETY: SSE2 is part of x86-64; the kernel touches words
        // `0..chunks * 16`, inside both slices (checked above).
        Isa::Sse2 => unsafe { x86::wdec_rows_sse2(top, bot, chunks, unit, w14, enc) },
        #[cfg(target_arch = "x86_64")]
        // SAFETY: as above, and AVX2 was detected before `Isa::Avx2` is chosen.
        Isa::Avx2 => unsafe { x86::wdec_rows_avx2(top, bot, chunks, unit, w14, enc) },
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

// ---------------------------------------------------------------------------
// Image::to_rgba_f32 for HALF planes.

/// Interleave HALF planes as RGBA floats (alpha 1.0 when `a` is `None`):
/// `out[4 i + k] = f16_to_f32(plane_k[i])`. Returns `false` when no SIMD
/// family is in use.
pub(crate) fn rgba_f16(
    isa: Isa,
    r: &[u16],
    g: &[u16],
    b: &[u16],
    a: Option<&[u16]>,
    out: &mut [f32],
) -> bool {
    let n = r.len();
    assert!(g.len() == n && b.len() == n && a.is_none_or(|a| a.len() == n));
    assert!(out.len() == 4 * n);
    match isa {
        #[cfg(target_arch = "x86_64")]
        // SAFETY: SSE2 is part of x86-64; lengths checked above.
        Isa::Sse2 => unsafe { x86::rgba_f16_sse2(r, g, b, a, out) },
        #[cfg(target_arch = "x86_64")]
        // SAFETY: as above, and AVX2 + F16C were detected.
        Isa::Avx2 => unsafe { x86::rgba_f16_avx2(r, g, b, a, out) },
        _ => return false,
    }
    true
}

// ---------------------------------------------------------------------------
// DWA decode: lossy FLOAT channels (halves to float words).

/// `words[2 i]`, `words[2 i + 1]` = low and high 16 bits of
/// `dwa::imath_h2f_bits(halves[i])`, for a prefix of whole 8-sample groups
/// whose halves are all non-NaN (Imath keeps a NaN's payload unquieted; the
/// hardware conversion would quiet it). Returns the number of samples done;
/// the caller finishes from there with the scalar code. `words` holds at
/// least `2 * halves.len()` words.
pub(crate) fn dwa_f32_words(isa: Isa, halves: &[u16], words: &mut [u16]) -> usize {
    assert!(words.len() >= 2 * halves.len());
    match isa {
        #[cfg(target_arch = "x86_64")]
        // SAFETY: SSE2 is part of x86-64; `words` holds 2 * halves.len()
        // u16 (checked above), the same bytes as halves.len() floats, and
        // the kernel stores unaligned.
        Isa::Sse2 => unsafe { x86::dwa_f32_sse2(halves, words.as_mut_ptr().cast()) },
        #[cfg(target_arch = "x86_64")]
        // SAFETY: as above, and AVX2 + F16C were detected.
        Isa::Avx2 => unsafe { x86::dwa_f32_avx2(halves, words.as_mut_ptr().cast()) },
        _ => 0,
    }
}

/// [`dwa_f32_words`] into floats: `out[i] = f32::from_bits(
/// dwa::imath_h2f_bits(halves[i]))` for the same NaN-free prefix of whole
/// 8-sample groups. `out` holds at least `halves.len()` floats.
pub(crate) fn dwa_f32(isa: Isa, halves: &[u16], out: &mut [f32]) -> usize {
    assert!(out.len() >= halves.len());
    match isa {
        #[cfg(target_arch = "x86_64")]
        // SAFETY: SSE2 is part of x86-64; length checked above.
        Isa::Sse2 => unsafe { x86::dwa_f32_sse2(halves, out.as_mut_ptr()) },
        #[cfg(target_arch = "x86_64")]
        // SAFETY: as above, and AVX2 + F16C were detected.
        Isa::Avx2 => unsafe { x86::dwa_f32_avx2(halves, out.as_mut_ptr()) },
        _ => 0,
    }
}

// ---------------------------------------------------------------------------
// DWA encode: block load (+ colour transform) and coefficient rounding.

/// Load an 8x8 block of each component in `comps` (1 or 3; rows start at
/// `row_starts[r]`, columns `x0..x0 + 8` contiguous) as floats, then for a
/// triple apply the forward colour transform: the twin of
/// `dwa_encode::gather_scalar`. Triples must hold finite halves (they come
/// from the perceptual transfer table, whose outputs all are).
pub(crate) fn dwa_enc_gather(
    isa: Isa,
    comps: &[Vec<u16>],
    row_starts: &[usize; 8],
    x0: usize,
    blk: &mut [[f32; 64]; 3],
) -> bool {
    let n = comps.len();
    assert!(n == 1 || n == 3);
    for c in comps {
        assert!(row_starts.iter().all(|&s| s + x0 + 8 <= c.len()));
    }
    match isa {
        #[cfg(target_arch = "x86_64")]
        // SAFETY: SSE2 is part of x86-64; every read is inside a component
        // (checked above) and every write inside `blk`.
        Isa::Sse2 => unsafe { x86::dwa_enc_gather_sse2(comps, row_starts, x0, blk) },
        #[cfg(target_arch = "x86_64")]
        // SAFETY: as above, and AVX2 + F16C were detected.
        Isa::Avx2 => unsafe { x86::dwa_enc_gather_avx2(comps, row_starts, x0, blk) },
        _ => return false,
    }
    true
}

/// `src[i] = f32_to_f16(d[i])`, `src_f[i] = f16_to_f32(src[i])`: the twin
/// of `dwa_encode::round_scalar` (exact for every input, NaN included).
pub(crate) fn dwa_enc_round(
    isa: Isa,
    d: &[f32; 64],
    src: &mut [u16; 64],
    src_f: &mut [f32; 64],
) -> bool {
    match isa {
        #[cfg(target_arch = "x86_64")]
        // SAFETY: SSE2 is part of x86-64; fixed-size arrays.
        Isa::Sse2 => unsafe { x86::dwa_enc_round_sse2(d, src, src_f) },
        #[cfg(target_arch = "x86_64")]
        // SAFETY: AVX2 + F16C were detected; fixed-size arrays.
        Isa::Avx2 => unsafe { x86::dwa_enc_round_avx2(d, src, src_f) },
        _ => return false,
    }
    true
}

// ---------------------------------------------------------------------------
// B44 decode: a channel's 4x4 blocks.

/// Decode B44 blocks of one HALF channel (`nx` x `ny`, little-endian
/// `out`) starting at `data[ip]`, in raster block order, through the
/// optional transfer table `lut` (65536 entries): the twin of
/// `b44::block_scalar` applied block after block. Stops before the first
/// block with fewer than 16 bytes left (the caller's scalar loop finishes,
/// and reports truncation). Returns (blocks decoded, next offset).
pub(crate) fn b44_channel(
    isa: Isa,
    data: &[u8],
    ip: usize,
    nx: usize,
    ny: usize,
    lut: Option<&[u16]>,
    out: &mut [u8],
) -> (usize, usize) {
    assert!(out.len() == nx * ny * 2 && lut.is_none_or(|l| l.len() == 1 << 16));
    match isa {
        #[cfg(target_arch = "x86_64")]
        // SAFETY: AVX2 (and so SSSE3) was detected; every block read is
        // checked to have 16 bytes, every store lands in a cell of `out`.
        Isa::Avx2 => unsafe { x86::b44_channel_avx2(data, ip, nx, ny, lut, out) },
        // The field extraction needs `pshufb` (SSSE3): SSE2 stays scalar.
        _ => (0, ip),
    }
}

// ---------------------------------------------------------------------------
// ZIP/RLE encode: byte-plane split and predictor.

/// `zip::deinterleave` over a prefix of whole 16-pair (32-byte) steps:
/// even bytes to `dst[..half]`, odd bytes to `dst[half..]`. Returns the
/// byte pairs done (0 when no SIMD family is in use).
pub(crate) fn deinterleave(isa: Isa, src: &[u8], dst: &mut [u8]) -> usize {
    let n = src.len();
    assert!(dst.len() == n);
    let steps = n / 32;
    match isa {
        #[cfg(target_arch = "x86_64")]
        // SAFETY: SSE2 is part of x86-64; reads `src[..32 * steps]`, writes
        // `dst[..16 * steps]` and `dst[half..half + 16 * steps]`, all in
        // bounds (half = ceil(n / 2) and 16 * steps <= n / 2).
        Isa::Sse2 | Isa::Avx2 => unsafe { x86::deinterleave_sse2(src, dst, steps) },
        _ => return 0,
    }
    16 * steps
}

/// `zip::apply_predictor` over a suffix of `t` in 16-byte steps taken from
/// the end (each step reads the original bytes just before it, which no
/// earlier step has changed). Returns where the scalar loop must stop
/// (`t.len()` when no SIMD family is in use).
pub(crate) fn apply_predictor(isa: Isa, t: &mut [u8]) -> usize {
    let n = t.len();
    if n < 17 {
        return n;
    }
    let steps = (n - 1) / 16;
    match isa {
        #[cfg(target_arch = "x86_64")]
        // SAFETY: SSE2 is part of x86-64; steps start at n - 16 k >= 1, so
        // every read (one byte before the step) and write is inside `t`.
        Isa::Sse2 | Isa::Avx2 => unsafe { x86::apply_predictor_sse2(t, steps) },
        _ => return n,
    }
    n - 16 * steps
}

// ---------------------------------------------------------------------------
// B44 encode: the per-block shift search.

/// The twin of `b44::search_scalar` (`None` when the AVX2 kernel is not
/// in use; the search needs unsigned 16-bit max and 256-bit 32-bit lanes).
pub(crate) fn b44_search(isa: Isa, s: &[u16; 16]) -> Option<crate::compression::b44::Search> {
    match isa {
        #[cfg(target_arch = "x86_64")]
        // SAFETY: AVX2 was detected; fixed-size arrays.
        Isa::Avx2 => Some(unsafe { x86::b44_search_avx2(s) }),
        _ => None,
    }
}

// ---------------------------------------------------------------------------
// PXR24 encode: row deltas into byte planes.

/// The twin of `pxr24::enc_row`: `src` holds `w` little-endian samples of
/// type `ty`, `dst` receives `w` bytes per plane (2 for HALF, 3 for FLOAT,
/// 4 for UINT). Returns `false` when no SIMD family is in use.
pub(crate) fn pxr24_enc_row(
    isa: Isa,
    ty: crate::header::SampleType,
    src: &[u8],
    dst: &mut [u8],
) -> bool {
    use crate::header::SampleType;
    let (bytes, planes) = match ty {
        SampleType::F16 => (2, 2),
        SampleType::F32 => (4, 3),
        SampleType::U32 => (4, 4),
    };
    let w = src.len() / bytes;
    assert!(src.len() == bytes * w && dst.len() == planes * w);
    match isa {
        #[cfg(target_arch = "x86_64")]
        // SAFETY: SSE2 is part of x86-64; lengths checked above.
        Isa::Sse2 | Isa::Avx2 => unsafe {
            if bytes == 2 {
                x86::pxr24_enc_16_sse2(src, dst, w);
            } else {
                x86::pxr24_enc_32_sse2(src, dst, w, planes);
            }
        },
        _ => return false,
    }
    true
}

/// `pxr24::f32_to_f24` on four values (test hook for the exhaustive check).
#[cfg(test)]
pub(crate) fn f24_4(isa: Isa, v: [u32; 4]) -> [u32; 4] {
    match isa {
        #[cfg(target_arch = "x86_64")]
        Isa::Sse2 | Isa::Avx2 => x86::f24_4_test(v),
        _ => v.map(crate::compression::pxr24::f32_to_f24),
    }
}

// ---------------------------------------------------------------------------
// RLE encode: run and literal scans.

/// Advance `end` over `data[start]`'s repeat run 16 positions at a time
/// while every position stays inside the data and the 128-byte cap; stop
/// at the first differing byte or before a step that could cross either
/// limit. `rle::run_end` finishes from the returned position (and is the
/// oracle: it gives the same final end from any position this returns).
pub(crate) fn rle_run_end(isa: Isa, data: &[u8], start: usize, end: usize) -> usize {
    assert!(start < data.len() && end >= start && end <= data.len());
    match isa {
        #[cfg(target_arch = "x86_64")]
        // SAFETY: SSE2 is part of x86-64; every 16-byte load is checked to
        // end inside `data`.
        Isa::Sse2 | Isa::Avx2 => unsafe { x86::rle_run_end_sse2(data, start, end) },
        _ => end,
    }
}

/// Advance `end` over a literal run 16 positions at a time while each
/// position has two bytes after it and stays under the 127-byte cap; stop
/// at the first position that starts three equal bytes. `rle::literal_end`
/// finishes (and is the oracle).
pub(crate) fn rle_literal_end(isa: Isa, data: &[u8], start: usize, end: usize) -> usize {
    assert!(start < data.len() && end >= start && end <= data.len());
    match isa {
        #[cfg(target_arch = "x86_64")]
        // SAFETY: SSE2 is part of x86-64; every load (up to end + 18) is
        // checked to end inside `data`.
        Isa::Sse2 | Isa::Avx2 => unsafe { x86::rle_literal_end_sse2(data, start, end) },
        _ => end,
    }
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
        d: &mut [[f32; 64]; 3],
    ) -> bool {
        if raster[..n].iter().any(|z| any_nonfinite(z)) {
            return false;
        }
        for c in 0..n {
            if zr[c] == ZR_CONSTANT {
                d[c] = flat(raster[c][0]);
            } else {
                h2f64_sse2(&raster[c], &mut d[c]);
                idct_sse2(&mut d[c], usize::from(zr[c]));
            }
        }
        if n == 3 {
            csc_sse2(d);
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
        d: &mut [[f32; 64]; 3],
    ) -> bool {
        if raster[..n].iter().any(|z| any_nonfinite(z)) {
            return false;
        }
        for c in 0..n {
            if zr[c] == ZR_CONSTANT {
                d[c] = flat(raster[c][0]);
            } else {
                h2f64_f16c(&raster[c], &mut d[c]);
                idct_avx2(&mut d[c], usize::from(zr[c]));
            }
        }
        if n == 3 {
            csc_avx2(d);
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
         $add:ident, $sub:ident, $and:ident, $xor:ident, $cmpgt:ident, $srai:ident,
         $srli:ident, $set1:ident, $de1:ident, $il1:ident, $de2:ident, $il2:ident) => {
            /// One 2x2 group step per lane: the inverse transform
            /// (`ENC == false`) or the forward one.
            #[target_feature(enable = $feat)]
            unsafe fn $name<const W14: bool, const UNIT: usize, const ENC: bool>(
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
                // `piz::wenc14` / `wenc16` (floor halving without a 17th bit:
                // (a >> 1) + (b >> 1) + (a & b & 1)).
                #[inline]
                #[target_feature(enable = $feat)]
                unsafe fn enc<const W14: bool>(a: $V, b: $V) -> ($V, $V) {
                    let one = $set1(1);
                    if W14 {
                        let m = $add($add($srai::<1>(a), $srai::<1>(b)), $and($and(a, b), one));
                        (m, $sub(a, b))
                    } else {
                        let top = $set1(-0x8000);
                        let ao = $xor(a, top);
                        let m = $add($add($srli::<1>(ao), $srli::<1>(b)), $and($and(ao, b), one));
                        // ao < b (unsigned) <=> a < b ^ 0x8000 (signed).
                        let lt = $cmpgt($xor(b, top), a);
                        ($add(m, $and(lt, top)), $sub(ao, b))
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
                    let (a, bv, cv, dv) = if ENC {
                        let (i00, i01) = enc::<W14>(ta, tb);
                        let (i10, i11) = enc::<W14>(ba, bb);
                        let (a, cv) = enc::<W14>(i00, i10);
                        let (bv, dv) = enc::<W14>(i01, i11);
                        (a, bv, cv, dv)
                    } else {
                        let (i00, i10) = dec::<W14>(ta, ba);
                        let (i01, i11) = dec::<W14>(tb, bb);
                        let (a, bv) = dec::<W14>(i00, i01);
                        let (cv, dv) = dec::<W14>(i10, i11);
                        (a, bv, cv, dv)
                    };
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
        _mm_xor_si128,
        _mm_cmpgt_epi16,
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
        _mm256_xor_si256,
        _mm256_cmpgt_epi16,
        _mm256_srai_epi16,
        _mm256_srli_epi16,
        _mm256_set1_epi16,
        de1_256,
        il1_256,
        de2_256,
        il2_256
    );

    /// Pick the monomorphised kernel for (forward?, 14-bit?, unit).
    macro_rules! wav_call {
        ($k:ident, $enc:expr, $w14:expr, $unit:expr, $t:expr, $b:expr, $n:expr) => {
            match ($enc, $w14, $unit) {
                (false, true, 1) => $k::<true, 1, false>($t, $b, $n),
                (false, true, _) => $k::<true, 2, false>($t, $b, $n),
                (false, false, 1) => $k::<false, 1, false>($t, $b, $n),
                (false, false, _) => $k::<false, 2, false>($t, $b, $n),
                (true, true, 1) => $k::<true, 1, true>($t, $b, $n),
                (true, true, _) => $k::<true, 2, true>($t, $b, $n),
                (true, false, 1) => $k::<false, 1, true>($t, $b, $n),
                (true, false, _) => $k::<false, 2, true>($t, $b, $n),
            }
        };
    }

    pub(super) unsafe fn wdec_rows_sse2(
        top: &mut [u16],
        bot: &mut [u16],
        chunks: usize,
        unit: usize,
        w14: bool,
        enc: bool,
    ) {
        let (t, b) = (top.as_mut_ptr(), bot.as_mut_ptr());
        wav_call!(wdec_sse2, enc, w14, unit, t, b, chunks);
    }

    #[target_feature(enable = "avx2")]
    pub(super) unsafe fn wdec_rows_avx2(
        top: &mut [u16],
        bot: &mut [u16],
        chunks: usize,
        unit: usize,
        w14: bool,
        enc: bool,
    ) {
        // 32-word chunks, then at most one 16-word chunk.
        let (t, b) = (top.as_mut_ptr(), bot.as_mut_ptr());
        let wide = chunks / 2;
        wav_call!(wdec_avx2, enc, w14, unit, t, b, wide);
        if chunks % 2 == 1 {
            let (t, b) = (t.add(wide * 32), b.add(wide * 32));
            wav_call!(wdec_sse2, enc, w14, unit, t, b, 1);
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

    // --- RGBA interleave -----------------------------------------------------

    /// Scalar tail of the RGBA kernels.
    fn rgba_tail(r: &[u16], g: &[u16], b: &[u16], a: Option<&[u16]>, out: &mut [f32], from: usize) {
        for i in from..r.len() {
            out[4 * i] = crate::half::f16_to_f32(r[i]);
            out[4 * i + 1] = crate::half::f16_to_f32(g[i]);
            out[4 * i + 2] = crate::half::f16_to_f32(b[i]);
            out[4 * i + 3] = a.map_or(1.0, |a| crate::half::f16_to_f32(a[i]));
        }
    }

    /// True when any of the 8 halves in `v` is a NaN.
    #[inline(always)]
    unsafe fn any_nan8(v: __m128i) -> bool {
        // |h| > 0x7c00 (as signed 16-bit: |h| <= 0x7fff).
        let a = _mm_and_si128(v, _mm_set1_epi16(0x7fff));
        _mm_movemask_epi8(_mm_cmpgt_epi16(a, _mm_set1_epi16(0x7c00))) != 0
    }

    /// `o` must be valid for `h.len()` unaligned f32 stores.
    pub(super) unsafe fn dwa_f32_sse2(h: &[u16], o: *mut f32) -> usize {
        let z = _mm_setzero_si128();
        let mut i = 0;
        while i + 8 <= h.len() {
            let v = _mm_loadu_si128(h.as_ptr().add(i).cast());
            if any_nan8(v) {
                break;
            }
            _mm_storeu_ps(o.add(i), h2f4(_mm_unpacklo_epi16(v, z)));
            _mm_storeu_ps(o.add(i + 4), h2f4(_mm_unpackhi_epi16(v, z)));
            i += 8;
        }
        i
    }

    /// `o` must be valid for `h.len()` unaligned f32 stores.
    #[target_feature(enable = "avx2,f16c")]
    pub(super) unsafe fn dwa_f32_avx2(h: &[u16], o: *mut f32) -> usize {
        let mut i = 0;
        while i + 8 <= h.len() {
            let v = _mm_loadu_si128(h.as_ptr().add(i).cast());
            if any_nan8(v) {
                break;
            }
            _mm256_storeu_ps(o.add(i), _mm256_cvtph_ps(v));
            i += 8;
        }
        i
    }

    pub(super) unsafe fn rgba_f16_sse2(
        r: &[u16],
        g: &[u16],
        b: &[u16],
        a: Option<&[u16]>,
        out: &mut [f32],
    ) {
        let n = r.len();
        let z = _mm_setzero_si128();
        let ld = |p: &[u16], i: usize| {
            h2f4(_mm_unpacklo_epi16(
                _mm_loadl_epi64(p.as_ptr().add(i).cast()),
                z,
            ))
        };
        let one = _mm_set1_ps(1.0);
        let mut i = 0;
        while i + 4 <= n {
            let (rv, gv, bv) = (ld(r, i), ld(g, i), ld(b, i));
            let av = match a {
                Some(a) => ld(a, i),
                None => one,
            };
            let t0 = _mm_unpacklo_ps(rv, gv);
            let t1 = _mm_unpackhi_ps(rv, gv);
            let t2 = _mm_unpacklo_ps(bv, av);
            let t3 = _mm_unpackhi_ps(bv, av);
            let o = out.as_mut_ptr().add(4 * i);
            _mm_storeu_ps(o, _mm_movelh_ps(t0, t2));
            _mm_storeu_ps(o.add(4), _mm_movehl_ps(t2, t0));
            _mm_storeu_ps(o.add(8), _mm_movelh_ps(t1, t3));
            _mm_storeu_ps(o.add(12), _mm_movehl_ps(t3, t1));
            i += 4;
        }
        rgba_tail(r, g, b, a, out, i);
    }

    #[target_feature(enable = "avx2,f16c")]
    pub(super) unsafe fn rgba_f16_avx2(
        r: &[u16],
        g: &[u16],
        b: &[u16],
        a: Option<&[u16]>,
        out: &mut [f32],
    ) {
        let n = r.len();
        let one = _mm256_set1_ps(1.0);
        let mut i = 0;
        while i + 8 <= n {
            let rv = _mm256_cvtph_ps(_mm_loadu_si128(r.as_ptr().add(i).cast()));
            let gv = _mm256_cvtph_ps(_mm_loadu_si128(g.as_ptr().add(i).cast()));
            let bv = _mm256_cvtph_ps(_mm_loadu_si128(b.as_ptr().add(i).cast()));
            let av = match a {
                Some(a) => _mm256_cvtph_ps(_mm_loadu_si128(a.as_ptr().add(i).cast())),
                None => one,
            };
            let t0 = _mm256_unpacklo_ps(rv, gv);
            let t1 = _mm256_unpackhi_ps(rv, gv);
            let t2 = _mm256_unpacklo_ps(bv, av);
            let t3 = _mm256_unpackhi_ps(bv, av);
            let u0 = _mm256_shuffle_ps::<0x44>(t0, t2);
            let u1 = _mm256_shuffle_ps::<0xee>(t0, t2);
            let u2 = _mm256_shuffle_ps::<0x44>(t1, t3);
            let u3 = _mm256_shuffle_ps::<0xee>(t1, t3);
            let o = out.as_mut_ptr().add(4 * i);
            _mm256_storeu_ps(o, _mm256_permute2f128_ps::<0x20>(u0, u1));
            _mm256_storeu_ps(o.add(8), _mm256_permute2f128_ps::<0x20>(u2, u3));
            _mm256_storeu_ps(o.add(16), _mm256_permute2f128_ps::<0x31>(u0, u1));
            _mm256_storeu_ps(o.add(24), _mm256_permute2f128_ps::<0x31>(u2, u3));
            i += 8;
        }
        rgba_tail(r, g, b, a, out, i);
    }

    // --- DWA encode ------------------------------------------------------------

    macro_rules! enc_csc {
        ($blk:expr, $n:literal, $load:ident, $store:ident, $add:ident, $sub:ident, $mul:ident,
         $div:ident, $set1:ident) => {{
            let blk = $blk;
            for k in 0..64 / $n {
                let r = $load(blk[0].as_ptr().add(k * $n));
                let g = $load(blk[1].as_ptr().add(k * $n));
                let b = $load(blk[2].as_ptr().add(k * $n));
                let y = $add(
                    $add($mul($set1(0.2126), r), $mul($set1(0.7152), g)),
                    $mul($set1(0.0722), b),
                );
                $store(blk[0].as_mut_ptr().add(k * $n), y);
                $store(
                    blk[1].as_mut_ptr().add(k * $n),
                    $div($sub(b, y), $set1(1.8556)),
                );
                $store(
                    blk[2].as_mut_ptr().add(k * $n),
                    $div($sub(r, y), $set1(1.5747)),
                );
            }
        }};
    }

    pub(super) unsafe fn dwa_enc_gather_sse2(
        comps: &[Vec<u16>],
        row_starts: &[usize; 8],
        x0: usize,
        blk: &mut [[f32; 64]; 3],
    ) {
        let z = _mm_setzero_si128();
        for (c, comp) in comps.iter().enumerate() {
            for (r, &s) in row_starts.iter().enumerate() {
                let v = _mm_loadu_si128(comp.as_ptr().add(s + x0).cast());
                _mm_storeu_ps(
                    blk[c].as_mut_ptr().add(r * 8),
                    h2f4(_mm_unpacklo_epi16(v, z)),
                );
                _mm_storeu_ps(
                    blk[c].as_mut_ptr().add(r * 8 + 4),
                    h2f4(_mm_unpackhi_epi16(v, z)),
                );
            }
        }
        if comps.len() == 3 {
            enc_csc!(
                blk,
                4,
                _mm_loadu_ps,
                _mm_storeu_ps,
                _mm_add_ps,
                _mm_sub_ps,
                _mm_mul_ps,
                _mm_div_ps,
                _mm_set1_ps
            );
        }
    }

    #[target_feature(enable = "avx2,f16c")]
    pub(super) unsafe fn dwa_enc_gather_avx2(
        comps: &[Vec<u16>],
        row_starts: &[usize; 8],
        x0: usize,
        blk: &mut [[f32; 64]; 3],
    ) {
        for (c, comp) in comps.iter().enumerate() {
            for (r, &s) in row_starts.iter().enumerate() {
                let v = _mm_loadu_si128(comp.as_ptr().add(s + x0).cast());
                _mm256_storeu_ps(blk[c].as_mut_ptr().add(r * 8), _mm256_cvtph_ps(v));
            }
        }
        if comps.len() == 3 {
            enc_csc!(
                blk,
                8,
                _mm256_loadu_ps,
                _mm256_storeu_ps,
                _mm256_add_ps,
                _mm256_sub_ps,
                _mm256_mul_ps,
                _mm256_div_ps,
                _mm256_set1_ps
            );
        }
    }

    pub(super) unsafe fn dwa_enc_round_sse2(
        d: &[f32; 64],
        src: &mut [u16; 64],
        src_f: &mut [f32; 64],
    ) {
        f2h64_sse2(d, src);
        h2f64_sse2(src, src_f);
    }

    #[target_feature(enable = "avx2,f16c")]
    pub(super) unsafe fn dwa_enc_round_avx2(
        d: &[f32; 64],
        src: &mut [u16; 64],
        src_f: &mut [f32; 64],
    ) {
        f2h64_f16c(d, src);
        h2f64_f16c(src, src_f);
    }

    // --- B44 decode -------------------------------------------------------------

    /// Per 16-bit lane of a block (lanes 0..8: rows 0 and 1, lanes 8..16:
    /// rows 2 and 3): which 6-bit field it adds (`-1`: lane 0 holds the
    /// first sample itself). Row `r`, column `k > 0` adds field
    /// `3 + 4 (k - 1) + r`; column 0 of row `r > 0` adds field `r - 1`.
    const B44_FIELD: [i32; 16] = [-1, 3, 7, 11, 0, 4, 8, 12, 1, 5, 9, 13, 2, 6, 10, 14];

    /// `pshufb` control: each lane gets the big-endian 16-bit window that
    /// holds its field (field `j` starts at bit `22 + 6 j`).
    const B44_CTRL: [u8; 32] = {
        let mut t = [0u8; 32];
        let mut l = 0;
        while l < 16 {
            let i = if B44_FIELD[l] < 0 {
                0
            } else {
                (22 + 6 * B44_FIELD[l] as usize) / 8
            };
            t[2 * l] = (i + 1) as u8;
            t[2 * l + 1] = i as u8;
            l += 1;
        }
        t
    };

    /// Multipliers that move every lane's field to bits 10..16 of the lane.
    const B44_MULT: [u16; 16] = {
        let mut t = [1u16; 16];
        let mut l = 0;
        while l < 16 {
            if B44_FIELD[l] >= 0 {
                t[l] = 1 << ((22 + 6 * B44_FIELD[l] as usize) % 8);
            }
            l += 1;
        }
        t
    };

    /// `finish`: negative halves lose the sign bit, the others invert.
    #[inline(always)]
    unsafe fn b44_finish(s: __m128i) -> __m128i {
        let m = _mm_srai_epi16::<15>(s);
        _mm_or_si128(
            _mm_and_si128(m, _mm_and_si128(s, _mm_set1_epi16(0x7fff))),
            _mm_andnot_si128(m, _mm_xor_si128(s, _mm_set1_epi16(-1))),
        )
    }

    /// One 14-byte block: rows 0-1 and rows 2-3, before `finish`.
    #[inline]
    #[target_feature(enable = "avx2")]
    unsafe fn b44_block14(p: *const u8) -> (__m128i, __m128i) {
        let v = _mm_loadu_si128(p.cast());
        let w0 = _mm_shuffle_epi8(v, _mm_loadu_si128(B44_CTRL.as_ptr().cast()));
        let w1 = _mm_shuffle_epi8(v, _mm_loadu_si128(B44_CTRL.as_ptr().add(16).cast()));
        let mult0 = _mm_loadu_si128(B44_MULT.as_ptr().cast());
        let mult1 = _mm_loadu_si128(B44_MULT.as_ptr().add(8).cast());
        let f0 = _mm_srli_epi16::<10>(_mm_mullo_epi16(w0, mult0));
        let f1 = _mm_srli_epi16::<10>(_mm_mullo_epi16(w1, mult1));
        let shift = u32::from(*p.add(2) >> 2);
        let cnt = _mm_cvtsi32_si128(shift as i32);
        let bias = _mm_set1_epi16((0x20u32 << shift) as u16 as i16);
        let d0 = _mm_sub_epi16(_mm_sll_epi16(f0, cnt), bias);
        let x1 = _mm_sub_epi16(_mm_sll_epi16(f1, cnt), bias);
        let lane0 = _mm_set_epi16(0, 0, 0, 0, 0, 0, 0, -1);
        let x0 = _mm_or_si128(_mm_and_si128(lane0, w0), _mm_andnot_si128(lane0, d0));
        // Prefix along each row (one row per 64-bit half).
        let mut y0 = _mm_add_epi16(x0, _mm_slli_epi64::<16>(x0));
        y0 = _mm_add_epi16(y0, _mm_slli_epi64::<32>(y0));
        let mut y1 = _mm_add_epi16(x1, _mm_slli_epi64::<16>(x1));
        y1 = _mm_add_epi16(y1, _mm_slli_epi64::<32>(y1));
        // Column-0 terms broadcast over their row, then summed over the rows
        // above.
        let col0 = _mm_set1_epi64x(0xffff);
        let mut c0 = _mm_and_si128(x0, col0);
        c0 = _mm_or_si128(c0, _mm_slli_epi64::<16>(c0));
        c0 = _mm_or_si128(c0, _mm_slli_epi64::<32>(c0));
        let mut c1 = _mm_and_si128(x1, col0);
        c1 = _mm_or_si128(c1, _mm_slli_epi64::<16>(c1));
        c1 = _mm_or_si128(c1, _mm_slli_epi64::<32>(c1));
        let above0 = _mm_slli_si128::<8>(c0);
        let both = _mm_add_epi16(c0, _mm_shuffle_epi32::<0x4e>(c0));
        let above1 = _mm_add_epi16(both, _mm_slli_si128::<8>(c1));
        (_mm_add_epi16(y0, above0), _mm_add_epi16(y1, above1))
    }

    // `_mm_storel_epi64` has no alignment requirement.
    #[allow(clippy::cast_ptr_alignment)]
    #[target_feature(enable = "avx2")]
    pub(super) unsafe fn b44_channel_avx2(
        data: &[u8],
        mut ip: usize,
        nx: usize,
        ny: usize,
        lut: Option<&[u16]>,
        out: &mut [u8],
    ) -> (usize, usize) {
        let nbx = nx.div_ceil(4);
        let total = nbx * ny.div_ceil(4);
        let mut b = 0;
        while b < total && ip + 16 <= data.len() {
            let p = data.as_ptr().add(ip);
            let (s0, s1) = if *p.add(2) >= (13 << 2) {
                let t0 = _mm_set1_epi16(i16::from_be_bytes([*p, *p.add(1)]));
                ip += 3;
                (t0, t0)
            } else {
                ip += 14;
                b44_block14(p)
            };
            let (s0, s1) = (b44_finish(s0), b44_finish(s1));
            let (x, y) = ((b % nbx) * 4, (b / nbx) * 4);
            if lut.is_none() && x + 4 <= nx && y + 4 <= ny {
                let o = out.as_mut_ptr();
                let row = |r: usize| o.add(((y + r) * nx + x) * 2).cast::<__m128i>();
                _mm_storel_epi64(row(0), s0);
                _mm_storel_epi64(row(1), _mm_srli_si128::<8>(s0));
                _mm_storel_epi64(row(2), s1);
                _mm_storel_epi64(row(3), _mm_srli_si128::<8>(s1));
            } else {
                let mut t = [0u16; 16];
                _mm_storeu_si128(t.as_mut_ptr().cast(), s0);
                _mm_storeu_si128(t.as_mut_ptr().add(8).cast(), s1);
                if let Some(l) = lut {
                    for v in &mut t {
                        *v = l[*v as usize];
                    }
                }
                for r in 0..(ny - y).min(4) {
                    let o = ((y + r) * nx + x) * 2;
                    for k in 0..(nx - x).min(4) {
                        out[o + 2 * k..o + 2 * k + 2].copy_from_slice(&t[r * 4 + k].to_le_bytes());
                    }
                }
            }
            b += 1;
        }
        (b, ip)
    }

    // --- ZIP/RLE encode -------------------------------------------------------

    pub(super) unsafe fn deinterleave_sse2(src: &[u8], dst: &mut [u8], steps: usize) {
        let half = src.len().div_ceil(2);
        let lo = _mm_set1_epi16(0x00ff);
        let (s, d) = (src.as_ptr(), dst.as_mut_ptr());
        for k in 0..steps {
            let v0 = _mm_loadu_si128(s.add(32 * k).cast());
            let v1 = _mm_loadu_si128(s.add(32 * k + 16).cast());
            let even = _mm_packus_epi16(_mm_and_si128(v0, lo), _mm_and_si128(v1, lo));
            let odd = _mm_packus_epi16(_mm_srli_epi16::<8>(v0), _mm_srli_epi16::<8>(v1));
            _mm_storeu_si128(d.add(16 * k).cast(), even);
            _mm_storeu_si128(d.add(half + 16 * k).cast(), odd);
        }
    }

    pub(super) unsafe fn apply_predictor_sse2(t: &mut [u8], steps: usize) {
        let n = t.len();
        let p = t.as_mut_ptr();
        let bias = _mm_set1_epi8(-128);
        for k in 1..=steps {
            let at = n - 16 * k;
            let cur = _mm_loadu_si128(p.add(at).cast());
            let prev = _mm_loadu_si128(p.add(at - 1).cast());
            _mm_storeu_si128(
                p.add(at).cast(),
                _mm_add_epi8(_mm_sub_epi8(cur, prev), bias),
            );
        }
    }

    // --- B44 encode -------------------------------------------------------------

    /// `b44::search_scalar` on 16 lanes: the sortable transform, the
    /// maximum, then per shift the rounded distances (32-bit lanes, one row
    /// per 128-bit half) and a single test that every delta fits.
    #[target_feature(enable = "avx2")]
    pub(super) unsafe fn b44_search_avx2(s: &[u16; 16]) -> crate::compression::b44::Search {
        let v = _mm256_loadu_si256(s.as_ptr().cast());
        let exp = _mm256_set1_epi16(0x7c00);
        let top = _mm256_set1_epi16(-0x8000);
        let infnan = _mm256_cmpeq_epi16(_mm256_and_si256(v, exp), exp);
        let neg = _mm256_srai_epi16::<15>(v);
        let fin = _mm256_or_si256(
            _mm256_and_si256(neg, _mm256_xor_si256(v, _mm256_set1_epi16(-1))),
            _mm256_andnot_si256(neg, _mm256_or_si256(v, top)),
        );
        let t = _mm256_or_si256(
            _mm256_and_si256(infnan, top),
            _mm256_andnot_si256(infnan, fin),
        );
        let mut tv = [0u16; 16];
        _mm256_storeu_si256(tv.as_mut_ptr().cast(), t);
        // Unsigned maximum, broadcast.
        let mut m = _mm_max_epu16(_mm256_castsi256_si128(t), _mm256_extracti128_si256::<1>(t));
        m = _mm_max_epu16(m, _mm_srli_si128::<8>(m));
        m = _mm_max_epu16(m, _mm_srli_si128::<4>(m));
        m = _mm_max_epu16(m, _mm_srli_si128::<2>(m));
        let t_max = i32::from(_mm_extract_epi16::<0>(m) as u16);
        let x = _mm256_sub_epi16(_mm256_set1_epi16(t_max as u16 as i16), t);
        // Doubled distances, rows 0-1 and rows 2-3.
        let x0 = _mm256_slli_epi32::<1>(_mm256_cvtepu16_epi32(_mm256_castsi256_si128(x)));
        let x1 = _mm256_slli_epi32::<1>(_mm256_cvtepu16_epi32(_mm256_extracti128_si256::<1>(x)));
        let one = _mm256_set1_epi32(1);
        let bias = _mm256_set1_epi32(32);
        let col = _mm256_setr_epi32(-1, 0, 0, 0, -1, 0, 0, 0);
        let outside = _mm256_set1_epi32(!63);
        let mut shift = 0;
        loop {
            // shift_and_round(x, shift) = (2x + 2^shift - 1 + bit) >> (shift + 1).
            let cnt = _mm_cvtsi32_si128(shift + 1);
            let a = _mm256_set1_epi32((1 << shift) - 1);
            let b0 = _mm256_and_si256(_mm256_srl_epi32(x0, cnt), one);
            let b1 = _mm256_and_si256(_mm256_srl_epi32(x1, cnt), one);
            let d0 = _mm256_srl_epi32(_mm256_add_epi32(_mm256_add_epi32(x0, a), b0), cnt);
            let d1 = _mm256_srl_epi32(_mm256_add_epi32(_mm256_add_epi32(x1, a), b1), cnt);
            // Along rows: d[c] - d[c + 1] (lane 3 of each row compares with
            // itself: 0, harmless). Down column 0: row r - row r + 1.
            let r0 = _mm256_sub_epi32(d0, _mm256_shuffle_epi32::<0b11_11_10_01>(d0));
            let r1 = _mm256_sub_epi32(d1, _mm256_shuffle_epi32::<0b11_11_10_01>(d1));
            let c0 = _mm256_sub_epi32(d0, _mm256_permute2x128_si256::<0x21>(d0, d1));
            let c1 = _mm256_sub_epi32(d1, _mm256_permute2x128_si256::<0x11>(d1, d1));
            let c0 = _mm256_and_si256(c0, col);
            let c1 = _mm256_and_si256(c1, col);
            // Every delta + 32 in 0..=63.
            let all = _mm256_or_si256(
                _mm256_or_si256(_mm256_add_epi32(r0, bias), _mm256_add_epi32(r1, bias)),
                _mm256_or_si256(_mm256_add_epi32(c0, bias), _mm256_add_epi32(c1, bias)),
            );
            if _mm256_testz_si256(all, outside) != 0 || shift >= 30 {
                let mut d = [0i32; 16];
                _mm256_storeu_si256(d.as_mut_ptr().cast(), d0);
                _mm256_storeu_si256(d.as_mut_ptr().add(8).cast(), d1);
                return crate::compression::b44::Search {
                    t: tv,
                    t_max,
                    shift,
                    d,
                };
            }
            shift += 1;
        }
    }

    // --- PXR24 encode -----------------------------------------------------------

    /// `pxr24::f32_to_f24` on four lanes (masks for the branches).
    #[inline(always)]
    unsafe fn f24(x: __m128i) -> __m128i {
        let sign = _mm_and_si128(x, _mm_set1_epi32(i32::MIN));
        let exp = _mm_and_si128(x, _mm_set1_epi32(0x7f80_0000));
        let man = _mm_and_si128(x, _mm_set1_epi32(0x007f_ffff));
        let z = _mm_setzero_si128();
        let one = _mm_set1_epi32(1);
        // Infinity / NaN: NaN keeps its high payload and stays a NaN.
        let special = _mm_cmpeq_epi32(exp, _mm_set1_epi32(0x7f80_0000));
        let m = _mm_srli_epi32::<8>(man);
        let man_set = _mm_andnot_si128(_mm_cmpeq_epi32(man, z), _mm_set1_epi32(-1));
        let m_zero = _mm_and_si128(_mm_cmpeq_epi32(m, z), one);
        let sv = _mm_or_si128(
            _mm_srli_epi32::<8>(exp),
            _mm_and_si128(man_set, _mm_or_si128(m, m_zero)),
        );
        // Finite: round the significand, unless that overflows the exponent.
        let em = _mm_or_si128(exp, man);
        let r = _mm_srli_epi32::<8>(_mm_add_epi32(em, _mm_and_si128(man, _mm_set1_epi32(0x80))));
        let over = _mm_cmpgt_epi32(r, _mm_set1_epi32(0x007f_7fff));
        let nv = _mm_or_si128(
            _mm_and_si128(over, _mm_srli_epi32::<8>(em)),
            _mm_andnot_si128(over, r),
        );
        let v = _mm_or_si128(_mm_and_si128(special, sv), _mm_andnot_si128(special, nv));
        _mm_or_si128(_mm_srli_epi32::<8>(sign), v)
    }

    #[cfg(test)]
    pub(super) fn f24_4_test(v: [u32; 4]) -> [u32; 4] {
        // SAFETY: SSE2 is baseline on x86-64.
        unsafe {
            let r = f24(_mm_loadu_si128(v.as_ptr().cast()));
            let mut o = [0u32; 4];
            _mm_storeu_si128(o.as_mut_ptr().cast(), r);
            o
        }
    }

    /// `pxr24::enc_row` for HALF: eight samples per step.
    pub(super) unsafe fn pxr24_enc_16_sse2(src: &[u8], dst: &mut [u8], w: usize) {
        let steps = w / 8;
        let lo8 = _mm_set1_epi16(0xff);
        let mut carry = _mm_setzero_si128();
        let d = dst.as_mut_ptr();
        for s in 0..steps {
            let v = _mm_loadu_si128(src.as_ptr().add(16 * s).cast());
            let prev = _mm_or_si128(_mm_slli_si128::<2>(v), carry);
            carry = _mm_srli_si128::<14>(v);
            let delta = _mm_sub_epi16(v, prev);
            let hi = _mm_packus_epi16(_mm_srli_epi16::<8>(delta), delta);
            let lo = _mm_packus_epi16(_mm_and_si128(delta, lo8), delta);
            _mm_storel_epi64(d.add(8 * s).cast(), hi);
            _mm_storel_epi64(d.add(w + 8 * s).cast(), lo);
        }
        let mut prev = if steps > 0 {
            u16::from_le_bytes([src[16 * steps - 2], src[16 * steps - 1]])
        } else {
            0
        };
        for x in 8 * steps..w {
            let v = u16::from_le_bytes([src[2 * x], src[2 * x + 1]]);
            let b = v.wrapping_sub(prev).to_be_bytes();
            prev = v;
            dst[x] = b[0];
            dst[w + x] = b[1];
        }
    }

    /// `pxr24::enc_row` for FLOAT (`planes == 3`) and UINT (`planes == 4`):
    /// eight samples per step.
    pub(super) unsafe fn pxr24_enc_32_sse2(src: &[u8], dst: &mut [u8], w: usize, planes: usize) {
        let steps = w / 8;
        let f24_on = planes == 3;
        let lo8 = _mm_set1_epi32(0xff);
        let mut carry = _mm_setzero_si128();
        let d = dst.as_mut_ptr();
        for s in 0..steps {
            let mut v0 = _mm_loadu_si128(src.as_ptr().add(32 * s).cast());
            let mut v1 = _mm_loadu_si128(src.as_ptr().add(32 * s + 16).cast());
            if f24_on {
                v0 = f24(v0);
                v1 = f24(v1);
            }
            let p0 = _mm_or_si128(_mm_slli_si128::<4>(v0), carry);
            let p1 = _mm_or_si128(_mm_slli_si128::<4>(v1), _mm_srli_si128::<12>(v0));
            carry = _mm_srli_si128::<12>(v1);
            let (d0, d1) = (_mm_sub_epi32(v0, p0), _mm_sub_epi32(v1, p1));
            // Plane k holds byte (planes - 1 - k) of each delta, counting
            // from the least significant.
            for k in 0..planes {
                let sh = _mm_cvtsi32_si128((8 * (planes - 1 - k)) as i32);
                let b0 = _mm_and_si128(_mm_srl_epi32(d0, sh), lo8);
                let b1 = _mm_and_si128(_mm_srl_epi32(d1, sh), lo8);
                let p = _mm_packs_epi32(b0, b1);
                _mm_storel_epi64(d.add(k * w + 8 * s).cast(), _mm_packus_epi16(p, p));
            }
        }
        let mut prev = 0u32;
        if steps > 0 {
            let o = 32 * steps;
            prev = u32::from_le_bytes([src[o - 4], src[o - 3], src[o - 2], src[o - 1]]);
            if f24_on {
                prev = crate::compression::pxr24::f32_to_f24(prev);
            }
        }
        for x in 8 * steps..w {
            let b = &src[4 * x..4 * x + 4];
            let mut v = u32::from_le_bytes([b[0], b[1], b[2], b[3]]);
            if f24_on {
                v = crate::compression::pxr24::f32_to_f24(v);
            }
            let dv = v.wrapping_sub(prev).to_be_bytes();
            prev = v;
            for k in 0..planes {
                dst[k * w + x] = dv[4 - planes + k];
            }
        }
    }

    // --- RLE encode -------------------------------------------------------------

    pub(super) unsafe fn rle_run_end_sse2(data: &[u8], start: usize, mut end: usize) -> usize {
        let p = data.as_ptr();
        let v = _mm_set1_epi8(data[start] as i8);
        // Positions end..end + 16 must satisfy `pos - start < 128`.
        while end + 16 <= data.len() && end + 15 - start < 128 {
            let m = _mm_movemask_epi8(_mm_cmpeq_epi8(_mm_loadu_si128(p.add(end).cast()), v)) as u32;
            if m != 0xffff {
                return end + (!m).trailing_zeros() as usize;
            }
            end += 16;
        }
        end
    }

    pub(super) unsafe fn rle_literal_end_sse2(data: &[u8], start: usize, mut end: usize) -> usize {
        let p = data.as_ptr();
        // Positions end..end + 16 need two bytes after them and
        // `pos - start < 127`.
        while end + 18 <= data.len() && end + 15 - start < 127 {
            let a = _mm_loadu_si128(p.add(end).cast());
            let b = _mm_loadu_si128(p.add(end + 1).cast());
            let c = _mm_loadu_si128(p.add(end + 2).cast());
            let m = _mm_movemask_epi8(_mm_and_si128(_mm_cmpeq_epi8(a, b), _mm_cmpeq_epi8(b, c)));
            if m != 0 {
                return end + m.trailing_zeros() as usize;
            }
            end += 16;
        }
        end
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
                assert!(dwa_block(
                    isa,
                    &raster,
                    zr,
                    n,
                    &mut got,
                    &mut [[f32::NAN; 64]; 3]
                ));
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
                            assert!(dwa_block(
                                isa,
                                &raster,
                                zr,
                                1,
                                &mut got,
                                &mut [[-1e30; 64]; 3]
                            ));
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
                assert!(!dwa_block(
                    isa,
                    &raster,
                    [0, 0, 0],
                    3,
                    &mut got,
                    &mut [[0.0; 64]; 3]
                ));
                assert_eq!(got, [[0u16; 64]; 3]);
            }
        }
        assert!(!dwa_block(
            Isa::Scalar,
            &[[0; 64]; 3],
            [0; 3],
            1,
            &mut [[0; 64]; 3],
            &mut [[0.0; 64]; 3]
        ));
    }
}
