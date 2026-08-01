//! VP9 inter prediction primitives (ISO/VP9 §8.5 / libvpx `vp9_reconinter.c` +
//! `vpx_dsp/vpx_convolve.c` + `vp9_filter.c`).
//!
//! Component 1 — the **8-tap sub-pixel convolution**. This is a fully isolated
//! primitive: given a reference plane, an integer block origin, a 1/16-pel
//! sub-pixel phase and a filter, it produces the motion-compensated block.
//! Reads are clamped to the visible plane `[0,w)×[0,h)`, which reproduces
//! libvpx's frame-border edge extension bit-for-bit without a border buffer.
// Wired into the reconstruction loop with the inter mode-info decoder (next).
#![allow(dead_code)]

/// `SUBPEL_TAPS` = 8, `FILTER_BITS` = 7, `SUBPEL_SHIFTS` = 16.
const TAPS: usize = 8;
const FILTER_BITS: u32 = 7;

/// The four switchable interpolation filters, indexed by `interp_filter`
/// (0 = EIGHTTAP, 1 = EIGHTTAP_SMOOTH, 2 = EIGHTTAP_SHARP, 3 = BILINEAR), each
/// `[phase 0..16][tap 0..8]`. Transcribed verbatim from libvpx `vp9_filter.c`.
pub const SUBPEL_FILTERS: [[[i32; TAPS]; 16]; 4] = [
    // EIGHTTAP (sub_pel_filters_8) — Lagrangian
    [
        [0, 0, 0, 128, 0, 0, 0, 0],
        [0, 1, -5, 126, 8, -3, 1, 0],
        [-1, 3, -10, 122, 18, -6, 2, 0],
        [-1, 4, -13, 118, 27, -9, 3, -1],
        [-1, 4, -16, 112, 37, -11, 4, -1],
        [-1, 5, -18, 105, 48, -14, 4, -1],
        [-1, 5, -19, 97, 58, -16, 5, -1],
        [-1, 6, -19, 88, 68, -18, 5, -1],
        [-1, 6, -19, 78, 78, -19, 6, -1],
        [-1, 5, -18, 68, 88, -19, 6, -1],
        [-1, 5, -16, 58, 97, -19, 5, -1],
        [-1, 4, -14, 48, 105, -18, 5, -1],
        [-1, 4, -11, 37, 112, -16, 4, -1],
        [-1, 3, -9, 27, 118, -13, 4, -1],
        [0, 2, -6, 18, 122, -10, 3, -1],
        [0, 1, -3, 8, 126, -5, 1, 0],
    ],
    // EIGHTTAP_SMOOTH (sub_pel_filters_8lp) — freqmultiplier 0.5
    [
        [0, 0, 0, 128, 0, 0, 0, 0],
        [-3, -1, 32, 64, 38, 1, -3, 0],
        [-2, -2, 29, 63, 41, 2, -3, 0],
        [-2, -2, 26, 63, 43, 4, -4, 0],
        [-2, -3, 24, 62, 46, 5, -4, 0],
        [-2, -3, 21, 60, 49, 7, -4, 0],
        [-1, -4, 18, 59, 51, 9, -4, 0],
        [-1, -4, 16, 57, 53, 12, -4, -1],
        [-1, -4, 14, 55, 55, 14, -4, -1],
        [-1, -4, 12, 53, 57, 16, -4, -1],
        [0, -4, 9, 51, 59, 18, -4, -1],
        [0, -4, 7, 49, 60, 21, -3, -2],
        [0, -4, 5, 46, 62, 24, -3, -2],
        [0, -4, 4, 43, 63, 26, -2, -2],
        [0, -3, 2, 41, 63, 29, -2, -2],
        [0, -3, 1, 38, 64, 32, -1, -3],
    ],
    // EIGHTTAP_SHARP (sub_pel_filters_8s) — DCT based
    [
        [0, 0, 0, 128, 0, 0, 0, 0],
        [-1, 3, -7, 127, 8, -3, 1, 0],
        [-2, 5, -13, 125, 17, -6, 3, -1],
        [-3, 7, -17, 121, 27, -10, 5, -2],
        [-4, 9, -20, 115, 37, -13, 6, -2],
        [-4, 10, -23, 108, 48, -16, 8, -3],
        [-4, 10, -24, 100, 59, -19, 9, -3],
        [-4, 11, -24, 90, 70, -21, 10, -4],
        [-4, 11, -23, 80, 80, -23, 11, -4],
        [-4, 10, -21, 70, 90, -24, 11, -4],
        [-3, 9, -19, 59, 100, -24, 10, -4],
        [-3, 8, -16, 48, 108, -23, 10, -4],
        [-2, 6, -13, 37, 115, -20, 9, -4],
        [-2, 5, -10, 27, 121, -17, 7, -3],
        [-1, 3, -6, 17, 125, -13, 5, -2],
        [0, 1, -3, 8, 127, -7, 3, -1],
    ],
    // BILINEAR (bilinear_filters)
    [
        [0, 0, 0, 128, 0, 0, 0, 0],
        [0, 0, 0, 120, 8, 0, 0, 0],
        [0, 0, 0, 112, 16, 0, 0, 0],
        [0, 0, 0, 104, 24, 0, 0, 0],
        [0, 0, 0, 96, 32, 0, 0, 0],
        [0, 0, 0, 88, 40, 0, 0, 0],
        [0, 0, 0, 80, 48, 0, 0, 0],
        [0, 0, 0, 72, 56, 0, 0, 0],
        [0, 0, 0, 64, 64, 0, 0, 0],
        [0, 0, 0, 56, 72, 0, 0, 0],
        [0, 0, 0, 48, 80, 0, 0, 0],
        [0, 0, 0, 40, 88, 0, 0, 0],
        [0, 0, 0, 32, 96, 0, 0, 0],
        [0, 0, 0, 24, 104, 0, 0, 0],
        [0, 0, 0, 16, 112, 0, 0, 0],
        [0, 0, 0, 8, 120, 0, 0, 0],
    ],
];

#[inline]
fn clip_pixel(v: i32, max: i32) -> u16 {
    v.clamp(0, max) as u16
}
#[inline]
fn round_pow2(v: i32, n: u32) -> i32 {
    (v + (1 << (n - 1))) >> n
}

thread_local! {
    /// 2-pass motion-comp intermediate scratch (max (64+7)×64), reused per block
    /// to avoid a heap allocation on every sub-pel inter prediction. Per-thread,
    /// so concurrent decoder instances don't contend.
    static MC_TMP: std::cell::RefCell<[u16; 71 * 64]> = const { std::cell::RefCell::new([0; 71 * 64]) };
}

/// A reference plane viewed for clamped reads.
pub struct RefPlane<'a> {
    pub buf: &'a [u16],
    pub stride: usize,
    pub w: i32,
    pub h: i32,
}

impl RefPlane<'_> {
    /// Edge-replicated pixel fetch (libvpx border extension equivalent).
    /// Defensive against degenerate/0-dim reference planes from malformed
    /// streams (the scalar border path; the interior hot path is AVX2 and is
    /// guarded by an explicit in-bounds check before dispatch).
    #[inline]
    fn px(&self, x: i32, y: i32) -> i32 {
        let cx = x.clamp(0, (self.w - 1).max(0)) as usize;
        let cy = y.clamp(0, (self.h - 1).max(0)) as usize;
        self.buf.get(cy * self.stride + cx).copied().unwrap_or(0) as i32
    }
}

/// A/B switch for the clamped-tile edge path (DEFAULT ON; set to disable).
#[cfg(target_arch = "x86_64")]
fn no_edge_tile() -> bool {
    static E: std::sync::OnceLock<bool> = std::sync::OnceLock::new();
    *E.get_or_init(|| std::env::var("VP9_NO_MC_EDGE_TILE").is_ok())
}

#[cfg(target_arch = "x86_64")]
#[inline]
fn has_avx2() -> bool {
    std::is_x86_feature_detected!("avx2")
}

/// AVX2 8-tap separable-convolution kernel, bit-identical to the scalar
/// `sum_k src[i + k*tap_stride] * f[k]`, rounded `>>7` and clamped to `[0,max]`.
/// Processes 8 outputs per iteration over `i in 0..n`; the `<8` tail is scalar.
/// `tap_stride == 1` is the horizontal pass; `== row_stride` the vertical pass.
///
/// # Safety
/// `src` must be readable for `i + 7*tap_stride + 7` u16s and `dst` writable for
/// `n` u16s; the caller guarantees this via an in-bounds (no edge-clamp) check.
#[cfg(target_arch = "x86_64")]
#[target_feature(enable = "avx2")]
#[inline]
unsafe fn conv8_avx2(
    src: *const u16,
    tap_stride: usize,
    f: &[i32; 8],
    dst: *mut u16,
    n: usize,
    max: i32,
    avg: bool,
) {
    use std::arch::x86_64::*;
    let round = _mm256_set1_epi32(64);
    let one = _mm256_set1_epi32(1);
    let maxv = _mm256_set1_epi32(max);
    let zero = _mm256_setzero_si256();
    // Taps paired (f0,f1)(f2,f3)(f4,f5)(f6,f7) as two i16 in each i32 lane, so
    // `madd_epi16` does two taps per instruction (i16×i16 → i32, no saturation, no
    // u16→u8 narrowing) — 4 madds replace 8 slow `mullo_epi32`. Bit-exact for
    // 8/10/12-bit: samples (≤4095) and taps (≤127) fit i16, products/sums are i32.
    let p01 = _mm256_set1_epi32((f[1] << 16) | (f[0] & 0xffff));
    let p23 = _mm256_set1_epi32((f[3] << 16) | (f[2] & 0xffff));
    let p45 = _mm256_set1_epi32((f[5] << 16) | (f[4] & 0xffff));
    let p67 = _mm256_set1_epi32((f[7] << 16) | (f[6] & 0xffff));
    let mut i = 0usize;
    while i + 8 <= n {
        let r0 = _mm_loadu_si128(src.add(i) as *const __m128i);
        let r1 = _mm_loadu_si128(src.add(i + tap_stride) as *const __m128i);
        let r2 = _mm_loadu_si128(src.add(i + 2 * tap_stride) as *const __m128i);
        let r3 = _mm_loadu_si128(src.add(i + 3 * tap_stride) as *const __m128i);
        let r4 = _mm_loadu_si128(src.add(i + 4 * tap_stride) as *const __m128i);
        let r5 = _mm_loadu_si128(src.add(i + 5 * tap_stride) as *const __m128i);
        let r6 = _mm_loadu_si128(src.add(i + 6 * tap_stride) as *const __m128i);
        let r7 = _mm_loadu_si128(src.add(i + 7 * tap_stride) as *const __m128i);
        // Interleave each tap-pair's two rows so `madd` pairs them per output lane;
        // low 128 → outputs 0..3, high 128 → outputs 4..7 (matches the pack below).
        let v01 = _mm256_set_m128i(_mm_unpackhi_epi16(r0, r1), _mm_unpacklo_epi16(r0, r1));
        let v23 = _mm256_set_m128i(_mm_unpackhi_epi16(r2, r3), _mm_unpacklo_epi16(r2, r3));
        let v45 = _mm256_set_m128i(_mm_unpackhi_epi16(r4, r5), _mm_unpacklo_epi16(r4, r5));
        let v67 = _mm256_set_m128i(_mm_unpackhi_epi16(r6, r7), _mm_unpacklo_epi16(r6, r7));
        let mut acc = _mm256_add_epi32(
            _mm256_add_epi32(_mm256_madd_epi16(v01, p01), _mm256_madd_epi16(v23, p23)),
            _mm256_add_epi32(_mm256_madd_epi16(v45, p45), _mm256_madd_epi16(v67, p67)),
        );
        acc = _mm256_srai_epi32::<7>(_mm256_add_epi32(acc, round));
        acc = _mm256_min_epi32(_mm256_max_epi32(acc, zero), maxv);
        if avg {
            // Compound: blend with the first reference already in dst,
            // `(pred + dst + 1) >> 1` — bit-identical to the scalar `put`.
            let d = _mm256_cvtepu16_epi32(_mm_loadu_si128(dst.add(i) as *const __m128i));
            acc = _mm256_srai_epi32::<1>(_mm256_add_epi32(_mm256_add_epi32(acc, d), one));
        }
        // pack i32x8 -> u16x8: packus gives [a0..3|a0..3 || a4..7|a4..7]; pull 64-bit lanes 0,2.
        let packed = _mm256_packus_epi32(acc, acc);
        let perm = _mm256_permute4x64_epi64::<0x08>(packed);
        _mm_storeu_si128(dst.add(i) as *mut __m128i, _mm256_castsi256_si128(perm));
        i += 8;
    }
    // 4-wide xmm block (the n%8==4 tail): chroma MC of 8×8 blocks is 4-wide and
    // previously fell to the fully-scalar loop below. Same pair-madd math on
    // 128-bit lanes — bit-identical; also serves the DECODER's chroma MC.
    if i + 4 <= n {
        let r0 = _mm_loadl_epi64(src.add(i) as *const __m128i);
        let r1 = _mm_loadl_epi64(src.add(i + tap_stride) as *const __m128i);
        let r2 = _mm_loadl_epi64(src.add(i + 2 * tap_stride) as *const __m128i);
        let r3 = _mm_loadl_epi64(src.add(i + 3 * tap_stride) as *const __m128i);
        let r4 = _mm_loadl_epi64(src.add(i + 4 * tap_stride) as *const __m128i);
        let r5 = _mm_loadl_epi64(src.add(i + 5 * tap_stride) as *const __m128i);
        let r6 = _mm_loadl_epi64(src.add(i + 6 * tap_stride) as *const __m128i);
        let r7 = _mm_loadl_epi64(src.add(i + 7 * tap_stride) as *const __m128i);
        let mut acc = _mm_add_epi32(
            _mm_add_epi32(
                _mm_madd_epi16(_mm_unpacklo_epi16(r0, r1), _mm256_castsi256_si128(p01)),
                _mm_madd_epi16(_mm_unpacklo_epi16(r2, r3), _mm256_castsi256_si128(p23)),
            ),
            _mm_add_epi32(
                _mm_madd_epi16(_mm_unpacklo_epi16(r4, r5), _mm256_castsi256_si128(p45)),
                _mm_madd_epi16(_mm_unpacklo_epi16(r6, r7), _mm256_castsi256_si128(p67)),
            ),
        );
        acc = _mm_srai_epi32::<7>(_mm_add_epi32(acc, _mm256_castsi256_si128(round)));
        acc = _mm_min_epi32(
            _mm_max_epi32(acc, _mm256_castsi256_si128(zero)),
            _mm256_castsi256_si128(maxv),
        );
        if avg {
            let d = _mm_cvtepu16_epi32(_mm_loadl_epi64(dst.add(i) as *const __m128i));
            acc = _mm_srai_epi32::<1>(_mm_add_epi32(
                _mm_add_epi32(acc, d),
                _mm256_castsi256_si128(one),
            ));
        }
        let packed = _mm_packus_epi32(acc, acc);
        _mm_storel_epi64(dst.add(i) as *mut __m128i, packed);
        i += 4;
    }
    while i < n {
        let mut sum = 0i32;
        for k in 0..8 {
            sum += *src.add(i + k * tap_stride) as i32 * f[k];
        }
        let v = ((sum + 64) >> 7).clamp(0, max);
        *dst.add(i) = if avg {
            ((v + *dst.add(i) as i32 + 1) >> 1) as u16
        } else {
            v as u16
        };
        i += 1;
    }
}

/// Fused AVX2 8×8 two-pass (horizontal + vertical) sub-pel prediction — the
/// hottest motion-search shape (millions of calls/encode). Identical math to
/// running `conv8_avx2` per row (same madd pairing, same `(x+64)>>7` rounding,
/// same [0,max] clamp, same u16 intermediate via saturating pack), but with the
/// filter registers built ONCE (vs 4 `set1` × 23 row calls), the 15-row
/// intermediate in a stack-local L1 buffer (no thread-local RefCell round-trip),
/// and no per-row call dispatch.
///
/// # Safety
/// AVX2 present; the (bx−3, by−3)..(bx+11, by+11) window lies inside the plane
/// (caller's in-bounds check) and `dst` holds 8 rows at `dst_stride`.
#[cfg(target_arch = "x86_64")]
#[target_feature(enable = "avx2")]
unsafe fn predict8x8_hv_avx2(
    refp: &RefPlane,
    bx: i32,
    by: i32,
    fx: &[i32; 8],
    fy: &[i32; 8],
    dst: *mut u16,
    dst_stride: usize,
    max: i32,
) {
    use std::arch::x86_64::*;
    let round = _mm256_set1_epi32(64);
    let maxv = _mm256_set1_epi32(max);
    let zero = _mm256_setzero_si256();
    let xp01 = _mm256_set1_epi32((fx[1] << 16) | (fx[0] & 0xffff));
    let xp23 = _mm256_set1_epi32((fx[3] << 16) | (fx[2] & 0xffff));
    let xp45 = _mm256_set1_epi32((fx[5] << 16) | (fx[4] & 0xffff));
    let xp67 = _mm256_set1_epi32((fx[7] << 16) | (fx[6] & 0xffff));
    // Horizontal pass: 15 rows (by−3 .. by+11) into a stack tmp, exactly as
    // conv8_avx2(tap_stride=1) computes them.
    let mut tmp = [0u16; 15 * 8];
    let base = refp.buf.as_ptr();
    for r in 0..15 {
        let s = base.add((by + r as i32 - 3) as usize * refp.stride + (bx - 3) as usize);
        let r0 = _mm_loadu_si128(s as *const __m128i);
        let r1 = _mm_loadu_si128(s.add(1) as *const __m128i);
        let r2 = _mm_loadu_si128(s.add(2) as *const __m128i);
        let r3 = _mm_loadu_si128(s.add(3) as *const __m128i);
        let r4 = _mm_loadu_si128(s.add(4) as *const __m128i);
        let r5 = _mm_loadu_si128(s.add(5) as *const __m128i);
        let r6 = _mm_loadu_si128(s.add(6) as *const __m128i);
        let r7 = _mm_loadu_si128(s.add(7) as *const __m128i);
        let v01 = _mm256_set_m128i(_mm_unpackhi_epi16(r0, r1), _mm_unpacklo_epi16(r0, r1));
        let v23 = _mm256_set_m128i(_mm_unpackhi_epi16(r2, r3), _mm_unpacklo_epi16(r2, r3));
        let v45 = _mm256_set_m128i(_mm_unpackhi_epi16(r4, r5), _mm_unpacklo_epi16(r4, r5));
        let v67 = _mm256_set_m128i(_mm_unpackhi_epi16(r6, r7), _mm_unpacklo_epi16(r6, r7));
        let mut acc = _mm256_add_epi32(
            _mm256_add_epi32(_mm256_madd_epi16(v01, xp01), _mm256_madd_epi16(v23, xp23)),
            _mm256_add_epi32(_mm256_madd_epi16(v45, xp45), _mm256_madd_epi16(v67, xp67)),
        );
        acc = _mm256_srai_epi32::<7>(_mm256_add_epi32(acc, round));
        acc = _mm256_min_epi32(_mm256_max_epi32(acc, zero), maxv);
        let packed = _mm256_packus_epi32(acc, acc);
        let perm = _mm256_permute4x64_epi64::<0x08>(packed);
        _mm_storeu_si128(
            tmp.as_mut_ptr().add(r * 8) as *mut __m128i,
            _mm256_castsi256_si128(perm),
        );
    }
    // Vertical pass over the tmp rows, same op sequence as conv8_avx2(tap_stride=8).
    let yp01 = _mm256_set1_epi32((fy[1] << 16) | (fy[0] & 0xffff));
    let yp23 = _mm256_set1_epi32((fy[3] << 16) | (fy[2] & 0xffff));
    let yp45 = _mm256_set1_epi32((fy[5] << 16) | (fy[4] & 0xffff));
    let yp67 = _mm256_set1_epi32((fy[7] << 16) | (fy[6] & 0xffff));
    for y in 0..8 {
        let s = tmp.as_ptr().add(y * 8);
        let r0 = _mm_loadu_si128(s as *const __m128i);
        let r1 = _mm_loadu_si128(s.add(8) as *const __m128i);
        let r2 = _mm_loadu_si128(s.add(16) as *const __m128i);
        let r3 = _mm_loadu_si128(s.add(24) as *const __m128i);
        let r4 = _mm_loadu_si128(s.add(32) as *const __m128i);
        let r5 = _mm_loadu_si128(s.add(40) as *const __m128i);
        let r6 = _mm_loadu_si128(s.add(48) as *const __m128i);
        let r7 = _mm_loadu_si128(s.add(56) as *const __m128i);
        let v01 = _mm256_set_m128i(_mm_unpackhi_epi16(r0, r1), _mm_unpacklo_epi16(r0, r1));
        let v23 = _mm256_set_m128i(_mm_unpackhi_epi16(r2, r3), _mm_unpacklo_epi16(r2, r3));
        let v45 = _mm256_set_m128i(_mm_unpackhi_epi16(r4, r5), _mm_unpacklo_epi16(r4, r5));
        let v67 = _mm256_set_m128i(_mm_unpackhi_epi16(r6, r7), _mm_unpacklo_epi16(r6, r7));
        let mut acc = _mm256_add_epi32(
            _mm256_add_epi32(_mm256_madd_epi16(v01, yp01), _mm256_madd_epi16(v23, yp23)),
            _mm256_add_epi32(_mm256_madd_epi16(v45, yp45), _mm256_madd_epi16(v67, yp67)),
        );
        acc = _mm256_srai_epi32::<7>(_mm256_add_epi32(acc, round));
        acc = _mm256_min_epi32(_mm256_max_epi32(acc, zero), maxv);
        let packed = _mm256_packus_epi32(acc, acc);
        let perm = _mm256_permute4x64_epi64::<0x08>(packed);
        _mm_storeu_si128(
            dst.add(y * dst_stride) as *mut __m128i,
            _mm256_castsi256_si128(perm),
        );
    }
}

/// Fused AVX2 8×8 single-pass (horizontal-only) sub-pel prediction: 8 rows of
/// the `tap_stride == 1` convolution with the filter registers built once.
/// Identical op sequence to `conv8_avx2` per row.
///
/// # Safety
/// AVX2 present; the (bx−3, by)..(bx+11, by+8) window lies inside the plane.
#[cfg(target_arch = "x86_64")]
#[target_feature(enable = "avx2")]
unsafe fn predict8x8_h_avx2(
    refp: &RefPlane,
    bx: i32,
    by: i32,
    fx: &[i32; 8],
    dst: *mut u16,
    dst_stride: usize,
    max: i32,
) {
    use std::arch::x86_64::*;
    let round = _mm256_set1_epi32(64);
    let maxv = _mm256_set1_epi32(max);
    let zero = _mm256_setzero_si256();
    let p01 = _mm256_set1_epi32((fx[1] << 16) | (fx[0] & 0xffff));
    let p23 = _mm256_set1_epi32((fx[3] << 16) | (fx[2] & 0xffff));
    let p45 = _mm256_set1_epi32((fx[5] << 16) | (fx[4] & 0xffff));
    let p67 = _mm256_set1_epi32((fx[7] << 16) | (fx[6] & 0xffff));
    let base = refp.buf.as_ptr();
    for y in 0..8 {
        let s = base.add((by + y) as usize * refp.stride + (bx - 3) as usize);
        let r0 = _mm_loadu_si128(s as *const __m128i);
        let r1 = _mm_loadu_si128(s.add(1) as *const __m128i);
        let r2 = _mm_loadu_si128(s.add(2) as *const __m128i);
        let r3 = _mm_loadu_si128(s.add(3) as *const __m128i);
        let r4 = _mm_loadu_si128(s.add(4) as *const __m128i);
        let r5 = _mm_loadu_si128(s.add(5) as *const __m128i);
        let r6 = _mm_loadu_si128(s.add(6) as *const __m128i);
        let r7 = _mm_loadu_si128(s.add(7) as *const __m128i);
        let v01 = _mm256_set_m128i(_mm_unpackhi_epi16(r0, r1), _mm_unpacklo_epi16(r0, r1));
        let v23 = _mm256_set_m128i(_mm_unpackhi_epi16(r2, r3), _mm_unpacklo_epi16(r2, r3));
        let v45 = _mm256_set_m128i(_mm_unpackhi_epi16(r4, r5), _mm_unpacklo_epi16(r4, r5));
        let v67 = _mm256_set_m128i(_mm_unpackhi_epi16(r6, r7), _mm_unpacklo_epi16(r6, r7));
        let mut acc = _mm256_add_epi32(
            _mm256_add_epi32(_mm256_madd_epi16(v01, p01), _mm256_madd_epi16(v23, p23)),
            _mm256_add_epi32(_mm256_madd_epi16(v45, p45), _mm256_madd_epi16(v67, p67)),
        );
        acc = _mm256_srai_epi32::<7>(_mm256_add_epi32(acc, round));
        acc = _mm256_min_epi32(_mm256_max_epi32(acc, zero), maxv);
        let packed = _mm256_packus_epi32(acc, acc);
        let perm = _mm256_permute4x64_epi64::<0x08>(packed);
        _mm_storeu_si128(
            dst.add(y as usize * dst_stride) as *mut __m128i,
            _mm256_castsi256_si128(perm),
        );
    }
}

/// Fused AVX2 8×8 single-pass (vertical-only) sub-pel prediction with a sliding
/// row window: consecutive output rows share 7 of their 8 input rows, so each
/// of the 15 source rows is loaded ONCE (vs 64 loads as 8 separate `conv8_avx2`
/// row calls). Same madd pairing / rounding / clamp — bit-identical.
///
/// # Safety
/// AVX2 present; rows (by−3)..(by+11) at `bx`..`bx+8` lie inside the plane.
#[cfg(target_arch = "x86_64")]
#[target_feature(enable = "avx2")]
unsafe fn predict8x8_v_avx2(
    refp: &RefPlane,
    bx: i32,
    by: i32,
    fy: &[i32; 8],
    dst: *mut u16,
    dst_stride: usize,
    max: i32,
) {
    use std::arch::x86_64::*;
    let round = _mm256_set1_epi32(64);
    let maxv = _mm256_set1_epi32(max);
    let zero = _mm256_setzero_si256();
    let p01 = _mm256_set1_epi32((fy[1] << 16) | (fy[0] & 0xffff));
    let p23 = _mm256_set1_epi32((fy[3] << 16) | (fy[2] & 0xffff));
    let p45 = _mm256_set1_epi32((fy[5] << 16) | (fy[4] & 0xffff));
    let p67 = _mm256_set1_epi32((fy[7] << 16) | (fy[6] & 0xffff));
    let base = refp.buf.as_ptr();
    let row = |r: i32| -> __m128i {
        _mm_loadu_si128(base.add((by + r) as usize * refp.stride + bx as usize) as *const __m128i)
    };
    // Sliding window of the 8 most recent source rows.
    let mut w = [
        row(-3),
        row(-2),
        row(-1),
        row(0),
        row(1),
        row(2),
        row(3),
        row(4),
    ];
    for y in 0..8usize {
        let v01 = _mm256_set_m128i(
            _mm_unpackhi_epi16(w[0], w[1]),
            _mm_unpacklo_epi16(w[0], w[1]),
        );
        let v23 = _mm256_set_m128i(
            _mm_unpackhi_epi16(w[2], w[3]),
            _mm_unpacklo_epi16(w[2], w[3]),
        );
        let v45 = _mm256_set_m128i(
            _mm_unpackhi_epi16(w[4], w[5]),
            _mm_unpacklo_epi16(w[4], w[5]),
        );
        let v67 = _mm256_set_m128i(
            _mm_unpackhi_epi16(w[6], w[7]),
            _mm_unpacklo_epi16(w[6], w[7]),
        );
        let mut acc = _mm256_add_epi32(
            _mm256_add_epi32(_mm256_madd_epi16(v01, p01), _mm256_madd_epi16(v23, p23)),
            _mm256_add_epi32(_mm256_madd_epi16(v45, p45), _mm256_madd_epi16(v67, p67)),
        );
        acc = _mm256_srai_epi32::<7>(_mm256_add_epi32(acc, round));
        acc = _mm256_min_epi32(_mm256_max_epi32(acc, zero), maxv);
        let packed = _mm256_packus_epi32(acc, acc);
        let perm = _mm256_permute4x64_epi64::<0x08>(packed);
        _mm_storeu_si128(
            dst.add(y * dst_stride) as *mut __m128i,
            _mm256_castsi256_si128(perm),
        );
        if y < 7 {
            w.copy_within(1.., 0);
            w[7] = row(5 + y as i32);
        }
    }
}

// ---- u8 search-domain score kernels (encoder-only) -------------------------
// For 8-bit content every luma pixel fits u8 exactly, and the two-pass subpel
// intermediates are clamped to [0,255] — so scoring in a u8 mirror of the
// source/reference planes is BIT-IDENTICAL to the u16 path while halving load
// traffic and using `psadbw` (a full 8-px SAD per instruction). These kernels
// fuse interpolate+SAD per row, so the prediction is never stored at all.

/// SSE2 `psadbw` 8×8 SAD over u8 planes — bit-identical to the u16 `sad8x8`.
///
/// # Safety
/// `s`/`r` must be readable for `7·stride + 8` bytes.
#[cfg(target_arch = "x86_64")]
#[target_feature(enable = "avx2")]
#[inline]
pub unsafe fn sad8x8_u8(s: *const u8, ss: usize, r: *const u8, rs: usize) -> u32 {
    use std::arch::x86_64::*;
    let mut acc = _mm_setzero_si128();
    for y in (0..8).step_by(2) {
        // Two 8-byte rows per xmm; psadbw sums |a−b| into two u16 lanes.
        let sv = _mm_set_epi64x(
            std::ptr::read_unaligned(s.add((y + 1) * ss) as *const i64),
            std::ptr::read_unaligned(s.add(y * ss) as *const i64),
        );
        let rv = _mm_set_epi64x(
            std::ptr::read_unaligned(r.add((y + 1) * rs) as *const i64),
            std::ptr::read_unaligned(r.add(y * rs) as *const i64),
        );
        acc = _mm_add_epi64(acc, _mm_sad_epu8(sv, rv));
    }
    (_mm_cvtsi128_si64(acc) as u32) + (_mm_extract_epi64::<1>(acc) as u32)
}

#[cfg(target_arch = "x86_64")]
mod u8score {
    // 16-lane pmaddubsw kernels: two 8-px rows per ymm (one per 128-lane).
    //
    // BIT-EXACTNESS PROOF (pinned by `pmaddubsw_preconditions_hold`): for every
    // VP9 filter at subpel phases 1..=15 —
    //   (1) every tap fits i8 (|f| <= 127);
    //   (2) each adjacent tap pair's positive sum <= 127+1  => the pmaddubsw
    //       u8*i8+u8*i8 pair product <= 255*128 = 32640 < 32767: NEVER saturates;
    //   (3) grouping (pair01 + pair45) and (pair23 + pair67): each group's
    //       positive-tap sum <= 128 => the first adds_epi16 level NEVER saturates;
    //   (4) the total negative reach >= -13770: NO negative saturation anywhere;
    //   (5) the final group1+group2 adds_epi16 (and the +64 rounding adds) can
    //       saturate at +32767 ONLY when the true sum >= 32767 - 64, and every
    //       such value shifts ((x+64)>>7) to >= 255, which the packus [0,255]
    //       clamp pins — the saturated and exact paths CONVERGE bit-identically.
    use super::RefPlane8;
    use std::arch::x86_64::*;

    /// Tap pairs as 16 replicated (i8,i8) lanes for pmaddubsw's signed operand.
    #[target_feature(enable = "avx2")]
    #[inline]
    unsafe fn tp(f: &[i32; 8]) -> [__m256i; 4] {
        std::array::from_fn(|k| {
            let lo = (f[2 * k] as i8 as u8) as u16;
            let hi = (f[2 * k + 1] as i8 as u8) as u16;
            _mm256_set1_epi16(((hi << 8) | lo) as i16)
        })
    }

    /// One 8-tap step for 2 independent rows (one per 128-lane): `m` holds, per
    /// lane, the pair-interleaved sample bytes for tap pairs 0..4; returns the
    /// 16 u8 outputs (row A in the low 8 bytes, row B in the high 8).
    #[target_feature(enable = "avx2")]
    #[inline]
    unsafe fn step2(m: [__m256i; 4], t: &[__m256i; 4]) -> __m128i {
        let m0 = _mm256_maddubs_epi16(m[0], t[0]);
        let m1 = _mm256_maddubs_epi16(m[1], t[1]);
        let m2 = _mm256_maddubs_epi16(m[2], t[2]);
        let m3 = _mm256_maddubs_epi16(m[3], t[3]);
        // Saturation-safe order: (01+45) and (23+67) are exact (proof (3));
        // the final add + rounding may saturate only in the clamps-anyway regime.
        let g1 = _mm256_adds_epi16(m0, m2);
        let g2 = _mm256_adds_epi16(m1, m3);
        let sum = _mm256_adds_epi16(g1, g2);
        let r = _mm256_adds_epi16(sum, _mm256_set1_epi16(64));
        let o = _mm256_srai_epi16::<7>(r);
        let p = _mm256_packus_epi16(o, o); // per-lane [0,255] clamp to u8
        _mm_unpacklo_epi64(_mm256_castsi256_si128(p), _mm256_extracti128_si256::<1>(p))
    }

    /// Horizontal pair-interleaves for 2 rows from one 16-byte window each
    /// (`p*` point at column bx-3; window bytes 0..16 cover taps for 8 outputs).
    #[target_feature(enable = "avx2")]
    #[inline]
    unsafe fn hwin2(p0: *const u8, p1: *const u8) -> [__m256i; 4] {
        let w = _mm256_set_m128i(
            _mm_loadu_si128(p1 as *const __m128i),
            _mm_loadu_si128(p0 as *const __m128i),
        );
        // pair k needs bytes (k+i, k+1+i), i = 0..8  =>  unpacklo(w>>k, w>>k+1)
        // (per-lane byte shifts keep the two rows independent).
        [
            _mm256_unpacklo_epi8(w, _mm256_srli_si256::<1>(w)),
            _mm256_unpacklo_epi8(_mm256_srli_si256::<2>(w), _mm256_srli_si256::<3>(w)),
            _mm256_unpacklo_epi8(_mm256_srli_si256::<4>(w), _mm256_srli_si256::<5>(w)),
            _mm256_unpacklo_epi8(_mm256_srli_si256::<6>(w), _mm256_srli_si256::<7>(w)),
        ]
    }

    /// SAD of 2 packed pred rows (from `step2`) against 2 source rows.
    #[target_feature(enable = "avx2")]
    #[inline]
    unsafe fn sad2(pred2: __m128i, s0: *const u8, s1: *const u8) -> u32 {
        let s = _mm_set_epi64x(
            std::ptr::read_unaligned(s1 as *const i64),
            std::ptr::read_unaligned(s0 as *const i64),
        );
        let d = _mm_sad_epu8(pred2, s);
        (_mm_cvtsi128_si32(d) as u32) + (_mm_extract_epi32::<2>(d) as u32)
    }

    /// Fused two-pass 8×8 subpel score, 2 rows per ymm throughout.
    ///
    /// # Safety
    /// AVX2; the (bx−3, by−3)..(bx+13, by+12) window lies inside the plane and
    /// `src` covers 8 rows at `src_stride`.
    #[target_feature(enable = "avx2")]
    pub(super) unsafe fn score_hv(
        refp: &RefPlane8,
        bx: i32,
        by: i32,
        fx: &[i32; 8],
        fy: &[i32; 8],
        src: *const u8,
        src_stride: usize,
    ) -> u32 {
        let tx = tp(fx);
        let ty = tp(fy);
        let base = refp.buf.as_ptr();
        let row = |r: i32| base.add((by + r) as usize * refp.stride + (bx - 3) as usize);
        // Horizontal pass into 15 u8 rows (2 rows per step; row 14 duplicated).
        let mut tmp = [0u8; 16 * 8];
        for i in 0..7 {
            let two = step2(hwin2(row(2 * i as i32 - 3), row(2 * i as i32 - 2)), &tx);
            _mm_storeu_si128(tmp.as_mut_ptr().add(16 * i) as *mut __m128i, two);
        }
        let last = step2(hwin2(row(11), row(11)), &tx);
        _mm_storel_epi64(tmp.as_mut_ptr().add(16 * 7) as *mut __m128i, last);
        // Vertical pass: pair-interleaves of consecutive tmp rows, built once.
        let mut inter = [_mm_setzero_si128(); 14];
        for (j, it) in inter.iter_mut().enumerate() {
            let a = _mm_loadl_epi64(tmp.as_ptr().add(8 * j) as *const __m128i);
            let b = _mm_loadl_epi64(tmp.as_ptr().add(8 * (j + 1)) as *const __m128i);
            *it = _mm_unpacklo_epi8(a, b);
        }
        let mut sad = 0u32;
        for y in (0..8usize).step_by(2) {
            let m: [__m256i; 4] =
                std::array::from_fn(|k| _mm256_set_m128i(inter[y + 1 + 2 * k], inter[y + 2 * k]));
            let two = step2(m, &ty);
            sad += sad2(two, src.add(y * src_stride), src.add((y + 1) * src_stride));
        }
        sad
    }

    /// Fused horizontal-only score, 2 rows per step.
    ///
    /// # Safety
    /// AVX2; (bx−3, by)..(bx+13, by+8) inside the plane; `src` covers 8 rows.
    #[target_feature(enable = "avx2")]
    pub(super) unsafe fn score_h(
        refp: &RefPlane8,
        bx: i32,
        by: i32,
        fx: &[i32; 8],
        src: *const u8,
        src_stride: usize,
    ) -> u32 {
        let tx = tp(fx);
        let base = refp.buf.as_ptr();
        let row = |r: i32| base.add((by + r) as usize * refp.stride + (bx - 3) as usize);
        let mut sad = 0u32;
        for y in (0..8).step_by(2) {
            let two = step2(hwin2(row(y), row(y + 1)), &tx);
            sad += sad2(
                two,
                src.add(y as usize * src_stride),
                src.add((y + 1) as usize * src_stride),
            );
        }
        sad
    }

    /// Exact squared error of 2 packed pred rows against 2 source rows:
    /// |a−b| as u8 (exact, ≤255), widened to i16, then madd(d,d) → i32 lanes.
    /// Max per call 128·255² < 2^24 — far inside i32/u32.
    #[target_feature(enable = "avx2")]
    #[inline]
    unsafe fn sse2(pred2: __m128i, s0: *const u8, s1: *const u8) -> u32 {
        let s = _mm_set_epi64x(
            std::ptr::read_unaligned(s1 as *const i64),
            std::ptr::read_unaligned(s0 as *const i64),
        );
        let d = _mm_or_si128(_mm_subs_epu8(pred2, s), _mm_subs_epu8(s, pred2));
        let d16 = _mm256_cvtepu8_epi16(d);
        let sq = _mm256_madd_epi16(d16, d16);
        let lo = _mm256_castsi256_si128(sq);
        let hi = _mm256_extracti128_si256::<1>(sq);
        let mut t = _mm_add_epi32(lo, hi);
        t = _mm_add_epi32(t, _mm_shuffle_epi32::<0b11_10_11_10>(t));
        t = _mm_add_epi32(t, _mm_shuffle_epi32::<0b01_01_01_01>(t));
        _mm_cvtsi128_si32(t) as u32
    }

    /// Fused two-pass 8×8 subpel SQUARED-ERROR (the shortlist `pred_sse` tile):
    /// identical interpolation to `score_hv`, SSE epilogue instead of SAD.
    ///
    /// # Safety
    /// Same window contract as `score_hv`.
    #[target_feature(enable = "avx2")]
    pub(super) unsafe fn sse_hv(
        refp: &RefPlane8,
        bx: i32,
        by: i32,
        fx: &[i32; 8],
        fy: &[i32; 8],
        src: *const u8,
        src_stride: usize,
    ) -> u32 {
        let tx = tp(fx);
        let ty = tp(fy);
        let base = refp.buf.as_ptr();
        let row = |r: i32| base.add((by + r) as usize * refp.stride + (bx - 3) as usize);
        let mut tmp = [0u8; 16 * 8];
        for i in 0..7 {
            let two = step2(hwin2(row(2 * i as i32 - 3), row(2 * i as i32 - 2)), &tx);
            _mm_storeu_si128(tmp.as_mut_ptr().add(16 * i) as *mut __m128i, two);
        }
        let last = step2(hwin2(row(11), row(11)), &tx);
        _mm_storel_epi64(tmp.as_mut_ptr().add(16 * 7) as *mut __m128i, last);
        let mut inter = [_mm_setzero_si128(); 14];
        for (j, it) in inter.iter_mut().enumerate() {
            let a = _mm_loadl_epi64(tmp.as_ptr().add(8 * j) as *const __m128i);
            let b = _mm_loadl_epi64(tmp.as_ptr().add(8 * (j + 1)) as *const __m128i);
            *it = _mm_unpacklo_epi8(a, b);
        }
        let mut sse = 0u32;
        for y in (0..8usize).step_by(2) {
            let m: [__m256i; 4] =
                std::array::from_fn(|k| _mm256_set_m128i(inter[y + 1 + 2 * k], inter[y + 2 * k]));
            let two = step2(m, &ty);
            sse += sse2(two, src.add(y * src_stride), src.add((y + 1) * src_stride));
        }
        sse
    }

    /// Fused horizontal-only squared error.
    ///
    /// # Safety
    /// Same window contract as `score_h`.
    #[target_feature(enable = "avx2")]
    pub(super) unsafe fn sse_h(
        refp: &RefPlane8,
        bx: i32,
        by: i32,
        fx: &[i32; 8],
        src: *const u8,
        src_stride: usize,
    ) -> u32 {
        let tx = tp(fx);
        let base = refp.buf.as_ptr();
        let row = |r: i32| base.add((by + r) as usize * refp.stride + (bx - 3) as usize);
        let mut sse = 0u32;
        for y in (0..8).step_by(2) {
            let two = step2(hwin2(row(y), row(y + 1)), &tx);
            sse += sse2(
                two,
                src.add(y as usize * src_stride),
                src.add((y + 1) as usize * src_stride),
            );
        }
        sse
    }

    /// Fused vertical-only squared error.
    ///
    /// # Safety
    /// Same window contract as `score_v`.
    #[target_feature(enable = "avx2")]
    pub(super) unsafe fn sse_v(
        refp: &RefPlane8,
        bx: i32,
        by: i32,
        fy: &[i32; 8],
        src: *const u8,
        src_stride: usize,
    ) -> u32 {
        let ty = tp(fy);
        let base = refp.buf.as_ptr();
        let row = |r: i32| {
            _mm_loadl_epi64(
                base.add((by + r) as usize * refp.stride + bx as usize) as *const __m128i
            )
        };
        let rows: [__m128i; 15] = std::array::from_fn(|j| row(j as i32 - 3));
        let inter: [__m128i; 14] = std::array::from_fn(|j| _mm_unpacklo_epi8(rows[j], rows[j + 1]));
        let mut sse = 0u32;
        for y in (0..8usize).step_by(2) {
            let m: [__m256i; 4] =
                std::array::from_fn(|k| _mm256_set_m128i(inter[y + 1 + 2 * k], inter[y + 2 * k]));
            let two = step2(m, &ty);
            sse += sse2(two, src.add(y * src_stride), src.add((y + 1) * src_stride));
        }
        sse
    }

    /// Full-pel squared error (no interpolation): ref bytes vs src bytes.
    ///
    /// # Safety
    /// AVX2; both windows cover 8 rows of 8 at their strides.
    #[target_feature(enable = "avx2")]
    pub(super) unsafe fn sse_copy(
        r: *const u8,
        rs: usize,
        src: *const u8,
        src_stride: usize,
    ) -> u32 {
        let mut sse = 0u32;
        for y in (0..8usize).step_by(2) {
            let pred = _mm_set_epi64x(
                std::ptr::read_unaligned(r.add((y + 1) * rs) as *const i64),
                std::ptr::read_unaligned(r.add(y * rs) as *const i64),
            );
            sse += sse2(pred, src.add(y * src_stride), src.add((y + 1) * src_stride));
        }
        sse
    }

    /// Fused vertical-only score: 15 8-byte row loads, pair-interleaves shared
    /// across output rows, 2 output rows per step.
    ///
    /// # Safety
    /// AVX2; rows (by−3)..(by+12) at bx..bx+8 inside the plane; `src` covers 8 rows.
    #[target_feature(enable = "avx2")]
    pub(super) unsafe fn score_v(
        refp: &RefPlane8,
        bx: i32,
        by: i32,
        fy: &[i32; 8],
        src: *const u8,
        src_stride: usize,
    ) -> u32 {
        let ty = tp(fy);
        let base = refp.buf.as_ptr();
        let row = |r: i32| {
            _mm_loadl_epi64(
                base.add((by + r) as usize * refp.stride + bx as usize) as *const __m128i
            )
        };
        let rows: [__m128i; 15] = std::array::from_fn(|j| row(j as i32 - 3));
        let inter: [__m128i; 14] = std::array::from_fn(|j| _mm_unpacklo_epi8(rows[j], rows[j + 1]));
        let mut sad = 0u32;
        for y in (0..8usize).step_by(2) {
            let m: [__m256i; 4] =
                std::array::from_fn(|k| _mm256_set_m128i(inter[y + 1 + 2 * k], inter[y + 2 * k]));
            let two = step2(m, &ty);
            sad += sad2(two, src.add(y * src_stride), src.add((y + 1) * src_stride));
        }
        sad
    }
}

#[cfg(target_arch = "x86_64")]
mod u8bilin {
    // 2-tap BILINEAR subpel SCORING kernels — the libvpx `sub_pixel_tree`
    // approach: the refinement RANKS candidates with a bilinear-filtered
    // prediction (taps (128−8p, 8p), FILTER_BITS=7); only the committed MC uses
    // the 8-tap filter. A bilinear output is a convex combination of two u8
    // samples, so every value stays in [0,255] with NO clamp and the single
    // pmaddubsw product ≤ 255·128 = 32640 < 32767 — no saturation anywhere.
    use super::RefPlane8;
    use std::arch::x86_64::*;

    /// (128−8p, 8p) replicated as 16 (i8,i8) lanes.
    #[target_feature(enable = "avx2")]
    #[inline]
    unsafe fn bt(phase: usize) -> __m256i {
        let a = (128 - 8 * phase as i32) as u8 as u16;
        let b = (8 * phase as i32) as u8 as u16;
        _mm256_set1_epi16(((b << 8) | a) as i16)
    }

    /// One bilinear step for 2 independent rows (one per 128-lane): `m` holds
    /// the (s[i], s[i+1]) interleaved bytes; returns 16 u8 outputs.
    #[target_feature(enable = "avx2")]
    #[inline]
    unsafe fn bstep2(m: __m256i, t: __m256i) -> __m128i {
        let v = _mm256_maddubs_epi16(m, t);
        let r = _mm256_srai_epi16::<7>(_mm256_add_epi16(v, _mm256_set1_epi16(64)));
        let p = _mm256_packus_epi16(r, r);
        _mm_unpacklo_epi64(_mm256_castsi256_si128(p), _mm256_extracti128_si256::<1>(p))
    }

    /// Horizontal (s[i], s[i+1]) interleaves for 2 rows from one 16B window each.
    #[target_feature(enable = "avx2")]
    #[inline]
    unsafe fn bwin2(p0: *const u8, p1: *const u8) -> __m256i {
        let w = _mm256_set_m128i(
            _mm_loadu_si128(p1 as *const __m128i),
            _mm_loadu_si128(p0 as *const __m128i),
        );
        _mm256_unpacklo_epi8(w, _mm256_srli_si256::<1>(w))
    }

    #[target_feature(enable = "avx2")]
    #[inline]
    unsafe fn sad2(pred2: __m128i, s0: *const u8, s1: *const u8) -> u32 {
        let s = _mm_set_epi64x(
            std::ptr::read_unaligned(s1 as *const i64),
            std::ptr::read_unaligned(s0 as *const i64),
        );
        let d = _mm_sad_epu8(pred2, s);
        (_mm_cvtsi128_si32(d) as u32) + (_mm_extract_epi32::<2>(d) as u32)
    }

    /// Fused bilinear two-pass 8×8 score. Window: (bx, by)..(bx+9, by+9).
    ///
    /// # Safety
    /// AVX2; window in-bounds; `src` covers 8 rows at `src_stride`.
    #[target_feature(enable = "avx2")]
    pub(super) unsafe fn score_hv(
        refp: &RefPlane8,
        bx: i32,
        by: i32,
        px: usize,
        py: usize,
        src: *const u8,
        src_stride: usize,
    ) -> u32 {
        let tx = bt(px);
        let ty = bt(py);
        let base = refp.buf.as_ptr();
        let row = |r: i32| base.add((by + r) as usize * refp.stride + bx as usize);
        // Horizontal pass into 9 u8 rows (4 pairs + 1 single).
        let mut tmp = [0u8; 16 * 5];
        for i in 0..4 {
            let two = bstep2(bwin2(row(2 * i as i32), row(2 * i as i32 + 1)), tx);
            _mm_storeu_si128(tmp.as_mut_ptr().add(16 * i) as *mut __m128i, two);
        }
        let last = bstep2(bwin2(row(8), row(8)), tx);
        _mm_storel_epi64(tmp.as_mut_ptr().add(16 * 4) as *mut __m128i, last);
        // Vertical pass: (row_j, row_{j+1}) interleaves, 2 output rows per step.
        let mut sad = 0u32;
        for y in (0..8usize).step_by(2) {
            let i0 = {
                let a = _mm_loadl_epi64(tmp.as_ptr().add(8 * y) as *const __m128i);
                let b = _mm_loadl_epi64(tmp.as_ptr().add(8 * (y + 1)) as *const __m128i);
                _mm_unpacklo_epi8(a, b)
            };
            let i1 = {
                let a = _mm_loadl_epi64(tmp.as_ptr().add(8 * (y + 1)) as *const __m128i);
                let b = _mm_loadl_epi64(tmp.as_ptr().add(8 * (y + 2)) as *const __m128i);
                _mm_unpacklo_epi8(a, b)
            };
            let two = bstep2(_mm256_set_m128i(i1, i0), ty);
            sad += sad2(two, src.add(y * src_stride), src.add((y + 1) * src_stride));
        }
        sad
    }

    /// Fused bilinear horizontal-only score. Window: (bx, by)..(bx+9, by+8).
    ///
    /// # Safety
    /// AVX2; window in-bounds; `src` covers 8 rows.
    #[target_feature(enable = "avx2")]
    pub(super) unsafe fn score_h(
        refp: &RefPlane8,
        bx: i32,
        by: i32,
        px: usize,
        src: *const u8,
        src_stride: usize,
    ) -> u32 {
        let tx = bt(px);
        let base = refp.buf.as_ptr();
        let row = |r: i32| base.add((by + r) as usize * refp.stride + bx as usize);
        let mut sad = 0u32;
        for y in (0..8).step_by(2) {
            let two = bstep2(bwin2(row(y), row(y + 1)), tx);
            sad += sad2(
                two,
                src.add(y as usize * src_stride),
                src.add((y + 1) as usize * src_stride),
            );
        }
        sad
    }

    /// Fused bilinear vertical-only score. Window: (bx, by)..(bx+8, by+9).
    ///
    /// # Safety
    /// AVX2; window in-bounds; `src` covers 8 rows.
    #[target_feature(enable = "avx2")]
    pub(super) unsafe fn score_v(
        refp: &RefPlane8,
        bx: i32,
        by: i32,
        py: usize,
        src: *const u8,
        src_stride: usize,
    ) -> u32 {
        let ty = bt(py);
        let base = refp.buf.as_ptr();
        let row = |r: i32| {
            _mm_loadl_epi64(
                base.add((by + r) as usize * refp.stride + bx as usize) as *const __m128i
            )
        };
        let rows: [__m128i; 9] = std::array::from_fn(|j| row(j as i32));
        let mut sad = 0u32;
        for y in (0..8usize).step_by(2) {
            let i0 = _mm_unpacklo_epi8(rows[y], rows[y + 1]);
            let i1 = _mm_unpacklo_epi8(rows[y + 1], rows[y + 2]);
            let two = bstep2(_mm256_set_m128i(i1, i0), ty);
            sad += sad2(two, src.add(y * src_stride), src.add((y + 1) * src_stride));
        }
        sad
    }
}

/// Scalar oracle for the bilinear scorer (the gate reference): 2-tap
/// (128−8p, 8p) two-pass, `(x·a + y·b + 64) >> 7` — convex, so no clamping.
#[cfg(target_arch = "x86_64")]
pub fn bilinear_score8x8_scalar(
    refb: &[u8],
    stride: usize,
    bx: usize,
    by: usize,
    px: usize,
    py: usize,
    src: &[u8],
    src_off: usize,
    src_stride: usize,
) -> u32 {
    let f = |a: u32, b: u32, p: usize| (a * (128 - 8 * p as u32) + b * (8 * p as u32) + 64) >> 7;
    let mut sad = 0u32;
    for y in 0..8usize {
        for x in 0..8usize {
            let at = |dy: usize, dx: usize| refb[(by + y + dy) * stride + bx + x + dx] as u32;
            let pred = match (px != 0, py != 0) {
                (false, false) => at(0, 0),
                (true, false) => f(at(0, 0), at(0, 1), px),
                (false, true) => f(at(0, 0), at(1, 0), py),
                (true, true) => {
                    let t0 = f(at(0, 0), at(0, 1), px);
                    let t1 = f(at(1, 0), at(1, 1), px);
                    f(t0, t1, py)
                }
            };
            let sv = src[src_off + y * src_stride + x] as u32;
            sad += sv.abs_diff(pred);
        }
    }
    sad
}

/// Fused u8 8×8 BILINEAR subpel score — the search-refinement ranking metric
/// (libvpx `sub_pixel_tree` semantics: 2-tap scoring, 8-tap only at commit).
/// Full-pel falls through to the exact `sad8x8_u8`.
///
/// # Safety
/// AVX2; the (bx, by)..(bx+9, by+9) window lies inside the plane.
#[cfg(target_arch = "x86_64")]
#[target_feature(enable = "avx2")]
pub unsafe fn subpel_bilinear_score8x8_u8(
    refp: &RefPlane8,
    bx: i32,
    by: i32,
    px: usize,
    py: usize,
    src: *const u8,
    src_stride: usize,
) -> u32 {
    match (px != 0, py != 0) {
        (false, false) => sad8x8_u8(
            src,
            src_stride,
            refp.buf
                .as_ptr()
                .add(by as usize * refp.stride + bx as usize),
            refp.stride,
        ),
        (true, false) => u8bilin::score_h(refp, bx, by, px, src, src_stride),
        (false, true) => u8bilin::score_v(refp, bx, by, py, src, src_stride),
        (true, true) => u8bilin::score_hv(refp, bx, by, px, py, src, src_stride),
    }
}

/// Four 8×8 SADs against one source tile in a single pass (libvpx `sad_x4d`):
/// the source rows are loaded ONCE and reused across all four references.
///
/// # Safety
/// AVX2 present; all five windows readable for `7·stride + 8` bytes.
#[cfg(target_arch = "x86_64")]
#[target_feature(enable = "avx2")]
pub unsafe fn sad8x8_x4_u8(src: *const u8, ss: usize, refs: [*const u8; 4], rs: usize) -> [u32; 4] {
    use std::arch::x86_64::*;
    let mut acc = [_mm_setzero_si128(); 4];
    for y in (0..8).step_by(2) {
        let sv = _mm_set_epi64x(
            std::ptr::read_unaligned(src.add((y + 1) * ss) as *const i64),
            std::ptr::read_unaligned(src.add(y * ss) as *const i64),
        );
        for (k, &r) in refs.iter().enumerate() {
            let rv = _mm_set_epi64x(
                std::ptr::read_unaligned(r.add((y + 1) * rs) as *const i64),
                std::ptr::read_unaligned(r.add(y * rs) as *const i64),
            );
            acc[k] = _mm_add_epi64(acc[k], _mm_sad_epu8(sv, rv));
        }
    }
    std::array::from_fn(|k| {
        (_mm_cvtsi128_si64(acc[k]) as u32) + (_mm_extract_epi64::<1>(acc[k]) as u32)
    })
}

/// A u8 mirror of a (luma) reference plane — the encoder search domain.
pub struct RefPlane8<'a> {
    pub buf: &'a [u8],
    pub stride: usize,
    pub w: i32,
    pub h: i32,
}

/// Fused u8 8×8 subpel SCORE (interpolate + SAD, no pred store) for the motion
/// search — bit-identical to `predict_block`(u16) + `sad8x8` for 8-bit content.
/// Caller guarantees the full filter window is in-bounds (same check as the
/// u16 AVX2 dispatch) and AVX2 is present.
///
/// # Safety
/// See the per-case kernels; window in-bounds, AVX2 present.
#[cfg(target_arch = "x86_64")]
#[target_feature(enable = "avx2")]
pub unsafe fn subpel_score8x8_u8(
    refp: &RefPlane8,
    bx: i32,
    by: i32,
    subpel_x: usize,
    subpel_y: usize,
    filter: usize,
    src: *const u8,
    src_stride: usize,
) -> u32 {
    let fx = &SUBPEL_FILTERS[filter][subpel_x];
    let fy = &SUBPEL_FILTERS[filter][subpel_y];
    match (subpel_x != 0, subpel_y != 0) {
        (false, false) => sad8x8_u8(
            src,
            src_stride,
            refp.buf
                .as_ptr()
                .add(by as usize * refp.stride + bx as usize),
            refp.stride,
        ),
        (true, false) => u8score::score_h(refp, bx, by, fx, src, src_stride),
        (false, true) => u8score::score_v(refp, bx, by, fy, src, src_stride),
        (true, true) => u8score::score_hv(refp, bx, by, fx, fy, src, src_stride),
    }
}

/// Fused u8 8×8 subpel SQUARED ERROR (interpolate + SSE, no pred store) for the
/// mode-shortlist `pred_sse` — bit-identical to `predict_block`(u16) + Σ(s−p)²
/// for 8-bit content. Same window contract as [`subpel_score8x8_u8`].
///
/// # Safety
/// Window in-bounds per the per-case kernels; AVX2 present.
#[cfg(target_arch = "x86_64")]
#[target_feature(enable = "avx2")]
pub unsafe fn subpel_sse8x8_u8(
    refp: &RefPlane8,
    bx: i32,
    by: i32,
    subpel_x: usize,
    subpel_y: usize,
    filter: usize,
    src: *const u8,
    src_stride: usize,
) -> u32 {
    let fx = &SUBPEL_FILTERS[filter][subpel_x];
    let fy = &SUBPEL_FILTERS[filter][subpel_y];
    match (subpel_x != 0, subpel_y != 0) {
        (false, false) => u8score::sse_copy(
            refp.buf
                .as_ptr()
                .add(by as usize * refp.stride + bx as usize),
            refp.stride,
            src,
            src_stride,
        ),
        (true, false) => u8score::sse_h(refp, bx, by, fx, src, src_stride),
        (false, true) => u8score::sse_v(refp, bx, by, fy, src, src_stride),
        (true, true) => u8score::sse_hv(refp, bx, by, fx, fy, src, src_stride),
    }
}

/// AVX2 averaging copy: `dst[i] = (src[i] + dst[i] + 1) >> 1` — the integer-pel
/// (no-subpel) compound case. `<8` tail is scalar.
///
/// # Safety
/// `src`/`dst` readable+writable for `n` u16s (caller checks bounds).
#[cfg(target_arch = "x86_64")]
#[target_feature(enable = "avx2")]
unsafe fn avg8_avx2(src: *const u16, dst: *mut u16, n: usize) {
    use std::arch::x86_64::*;
    let one = _mm256_set1_epi32(1);
    let mut i = 0usize;
    while i + 8 <= n {
        let sv = _mm256_cvtepu16_epi32(_mm_loadu_si128(src.add(i) as *const __m128i));
        let dv = _mm256_cvtepu16_epi32(_mm_loadu_si128(dst.add(i) as *const __m128i));
        let a = _mm256_srai_epi32::<1>(_mm256_add_epi32(_mm256_add_epi32(sv, dv), one));
        let packed = _mm256_packus_epi32(a, a);
        let perm = _mm256_permute4x64_epi64::<0x08>(packed);
        _mm_storeu_si128(dst.add(i) as *mut __m128i, _mm256_castsi256_si128(perm));
        i += 8;
    }
    while i < n {
        *dst.add(i) = ((*src.add(i) as i32 + *dst.add(i) as i32 + 1) >> 1) as u16;
        i += 1;
    }
}

/// AVX2 separable MC for an interior block (no frame-border clamp, no compound
/// averaging). Mirrors the four [`predict_block`] branches exactly.
///
/// # Safety
/// The full read window must lie inside the plane (caller checks bounds).
#[cfg(target_arch = "x86_64")]
#[target_feature(enable = "avx2")]
unsafe fn predict_block_avx2(
    refp: &RefPlane,
    bx: i32,
    by: i32,
    fx: &[i32; 8],
    fy: &[i32; 8],
    subx: bool,
    suby: bool,
    dst: &mut [u16],
    dst_stride: usize,
    w: usize,
    h: usize,
    max: i32,
    avg: bool,
) {
    let buf = refp.buf.as_ptr();
    let stride = refp.stride;
    let dptr = dst.as_mut_ptr();
    match (subx, suby) {
        (false, false) => {
            for y in 0..h {
                let s = buf.add((by as usize + y) * stride + bx as usize);
                let d = dptr.add(y * dst_stride);
                if avg {
                    avg8_avx2(s, d, w);
                } else {
                    std::ptr::copy_nonoverlapping(s, d, w);
                }
            }
        }
        (true, false) => {
            if w == 8 && h == 8 && !avg {
                predict8x8_h_avx2(refp, bx, by, fx, dptr, dst_stride, max);
                return;
            }
            for y in 0..h {
                let s = buf.add((by as usize + y) * stride + (bx - 3) as usize);
                conv8_avx2(s, 1, fx, dptr.add(y * dst_stride), w, max, avg);
            }
        }
        (false, true) => {
            if w == 8 && h == 8 && !avg {
                predict8x8_v_avx2(refp, bx, by, fy, dptr, dst_stride, max);
                return;
            }
            for y in 0..h {
                let s = buf.add((by + y as i32 - 3) as usize * stride + bx as usize);
                conv8_avx2(s, stride, fy, dptr.add(y * dst_stride), w, max, avg);
            }
        }
        (true, true) => {
            if w == 8 && h == 8 && !avg {
                // Fused hot path (the dominant search shape): identical math,
                // one call, filter regs hoisted, no scratch round-trip.
                predict8x8_hv_avx2(refp, bx, by, fx, fy, dptr, dst_stride, max);
                return;
            }
            MC_TMP.with(|cell| {
                let mut tmp = cell.borrow_mut();
                let tmp_h = h + TAPS - 1;
                let tptr = tmp.as_mut_ptr();
                // Intermediate horizontal pass is never averaged (avg only the
                // final write into dst).
                for r in 0..tmp_h {
                    let s = buf.add((by + r as i32 - 3) as usize * stride + (bx - 3) as usize);
                    conv8_avx2(s, 1, fx, tptr.add(r * w), w, max, false);
                }
                for y in 0..h {
                    conv8_avx2(
                        tptr.add(y * w) as *const u16,
                        w,
                        fy,
                        dptr.add(y * dst_stride),
                        w,
                        max,
                        avg,
                    );
                }
            });
        }
    }
}

// ---- aarch64 NEON: mirror of the AVX2 path -------------------------------
// NEON is the mandatory baseline on aarch64, so these are always reachable
// there. Each kernel performs the SAME integer math as the scalar reference
// (`(Σ src·f + 64) >> 7`, clamped to `[0,max]`), so it is bit-exact by
// construction; `conv8_neon_matches_scalar` is the regression gate (runs on an
// aarch64 target). Built/verified via `cargo build --target aarch64-*`.

#[cfg(target_arch = "aarch64")]
#[inline]
fn has_neon() -> bool {
    std::arch::is_aarch64_feature_detected!("neon")
}

/// NEON 8-tap separable-convolution kernel, bit-identical to the scalar
/// `Σ_k src[i + k*tap_stride] * f[k]`, rounded `>>7` and clamped to `[0,max]`.
/// Processes 4 outputs per iteration; the `<4` tail is scalar. `tap_stride == 1`
/// is the horizontal pass, `== row_stride` the vertical.
///
/// # Safety
/// `src` must be readable for `i + 7*tap_stride + 3` u16s and `dst` writable for
/// `n` u16s; the caller guarantees this via an in-bounds (no edge-clamp) check.
#[cfg(target_arch = "aarch64")]
#[target_feature(enable = "neon")]
unsafe fn conv8_neon(
    src: *const u16,
    tap_stride: usize,
    f: &[i32; 8],
    dst: *mut u16,
    n: usize,
    max: i32,
    avg: bool,
) {
    use std::arch::aarch64::*;
    let round = vdupq_n_s32(64);
    let one = vdupq_n_s32(1);
    let zero = vdupq_n_s32(0);
    let maxv = vdupq_n_s32(max);
    let mut i = 0usize;
    while i + 4 <= n {
        let mut acc = zero;
        // 8 taps: load 4 consecutive u16 (the k-th tap of 4 adjacent outputs),
        // zero-widen to u32 (values are non-negative samples ≤ max), and MAC by
        // the signed scalar tap. Bit-identical to the scalar inner loop.
        for k in 0..8 {
            let s = vreinterpretq_s32_u32(vmovl_u16(vld1_u16(src.add(i + k * tap_stride))));
            acc = vmlaq_n_s32(acc, s, f[k]);
        }
        // (Σ + 64) >> 7 with a signed (arithmetic) shift, then clamp [0,max].
        acc = vshrq_n_s32::<7>(vaddq_s32(acc, round));
        acc = vminq_s32(vmaxq_s32(acc, zero), maxv);
        if avg {
            // Compound: `(pred + dst + 1) >> 1`, bit-identical to scalar `put`.
            let d = vreinterpretq_s32_u32(vmovl_u16(vld1_u16(dst.add(i))));
            acc = vshrq_n_s32::<1>(vaddq_s32(vaddq_s32(acc, d), one));
        }
        // Narrow i32 (already in [0,max]) → u16 by truncation (exact: fits u16).
        vst1_u16(dst.add(i), vmovn_u32(vreinterpretq_u32_s32(acc)));
        i += 4;
    }
    while i < n {
        let mut sum = 0i32;
        for k in 0..8 {
            sum += *src.add(i + k * tap_stride) as i32 * f[k];
        }
        let v = ((sum + 64) >> 7).clamp(0, max);
        *dst.add(i) = if avg {
            ((v + *dst.add(i) as i32 + 1) >> 1) as u16
        } else {
            v as u16
        };
        i += 1;
    }
}

/// NEON averaging copy: `dst[i] = (src[i] + dst[i] + 1) >> 1` (integer-pel
/// compound). `<4` tail is scalar.
///
/// # Safety
/// `src`/`dst` readable+writable for `n` u16s (caller checks bounds).
#[cfg(target_arch = "aarch64")]
#[target_feature(enable = "neon")]
unsafe fn avg8_neon(src: *const u16, dst: *mut u16, n: usize) {
    use std::arch::aarch64::*;
    let one = vdupq_n_s32(1);
    let mut i = 0usize;
    while i + 4 <= n {
        let s = vreinterpretq_s32_u32(vmovl_u16(vld1_u16(src.add(i))));
        let d = vreinterpretq_s32_u32(vmovl_u16(vld1_u16(dst.add(i))));
        let a = vshrq_n_s32::<1>(vaddq_s32(vaddq_s32(s, d), one));
        vst1_u16(dst.add(i), vmovn_u32(vreinterpretq_u32_s32(a)));
        i += 4;
    }
    while i < n {
        *dst.add(i) = ((*src.add(i) as i32 + *dst.add(i) as i32 + 1) >> 1) as u16;
        i += 1;
    }
}

/// NEON separable MC for an interior block (no border clamp, no compound
/// averaging). Mirrors the four [`predict_block`] branches exactly.
///
/// # Safety
/// The full read window must lie inside the plane (caller checks bounds).
#[cfg(target_arch = "aarch64")]
#[target_feature(enable = "neon")]
unsafe fn predict_block_neon(
    refp: &RefPlane,
    bx: i32,
    by: i32,
    fx: &[i32; 8],
    fy: &[i32; 8],
    subx: bool,
    suby: bool,
    dst: &mut [u16],
    dst_stride: usize,
    w: usize,
    h: usize,
    max: i32,
    avg: bool,
) {
    let buf = refp.buf.as_ptr();
    let stride = refp.stride;
    let dptr = dst.as_mut_ptr();
    match (subx, suby) {
        (false, false) => {
            for y in 0..h {
                let s = buf.add((by as usize + y) * stride + bx as usize);
                let d = dptr.add(y * dst_stride);
                if avg {
                    avg8_neon(s, d, w);
                } else {
                    std::ptr::copy_nonoverlapping(s, d, w);
                }
            }
        }
        (true, false) => {
            for y in 0..h {
                let s = buf.add((by as usize + y) * stride + (bx - 3) as usize);
                conv8_neon(s, 1, fx, dptr.add(y * dst_stride), w, max, avg);
            }
        }
        (false, true) => {
            for y in 0..h {
                let s = buf.add((by + y as i32 - 3) as usize * stride + bx as usize);
                conv8_neon(s, stride, fy, dptr.add(y * dst_stride), w, max, avg);
            }
        }
        (true, true) => {
            MC_TMP.with(|cell| {
                let mut tmp = cell.borrow_mut();
                let tmp_h = h + TAPS - 1;
                let tptr = tmp.as_mut_ptr();
                // Intermediate pass is never averaged (avg only the final write).
                for r in 0..tmp_h {
                    let s = buf.add((by + r as i32 - 3) as usize * stride + (bx - 3) as usize);
                    conv8_neon(s, 1, fx, tptr.add(r * w), w, max, false);
                }
                for y in 0..h {
                    conv8_neon(
                        tptr.add(y * w) as *const u16,
                        w,
                        fy,
                        dptr.add(y * dst_stride),
                        w,
                        max,
                        avg,
                    );
                }
            });
        }
    }
}

/// Motion-compensate one block into `dst`. `(bx, by)` is the integer-pel block
/// origin in the reference plane (block position + the integer part of the MV);
/// `subpel_x/y` are the 1/16-pel fractional phases (0..16). `filter` selects the
/// kernel; `avg` averages into `dst` for the second reference of a compound.
#[allow(clippy::too_many_arguments)]
pub fn predict_block(
    refp: &RefPlane,
    bx: i32,
    by: i32,
    subpel_x: usize,
    subpel_y: usize,
    filter: usize,
    dst: &mut [u16],
    dst_stride: usize,
    w: usize,
    h: usize,
    avg: bool,
    max: i32,
) {
    let fx = &SUBPEL_FILTERS[filter][subpel_x];
    let fy = &SUBPEL_FILTERS[filter][subpel_y];

    // AVX2 fast path: interior block (no edge clamp), single-ref (no averaging).
    // The scalar branches below remain the bit-exact reference / fallback.
    #[cfg(target_arch = "x86_64")]
    {
        let (subx, suby) = (subpel_x != 0, subpel_y != 0);
        let (nl, nr) = if subx { (3, 4) } else { (0, 0) };
        let (nt, nb) = if suby { (3, 4) } else { (0, 0) };
        let in_bounds = bx - nl >= 0
            && bx + w as i32 + nr <= refp.w
            && by - nt >= 0
            && by + h as i32 + nb <= refp.h;
        if in_bounds && has_avx2() {
            // SAFETY: bounds checked above; AVX2 confirmed present.
            unsafe {
                predict_block_avx2(
                    refp, bx, by, fx, fy, subx, suby, dst, dst_stride, w, h, max, avg,
                );
            }
            return;
        }
        // EDGE BLOCKS: gather a clamped tile, then run the SAME fast kernel on
        // it. The scalar clamping convolve below is 3-12x slower than the AVX2
        // path (8x8 xy: 2277 vs 188 cyc/call), and although only ~10-14% of
        // blocks are edge blocks, that made them ~46% of all inter-prediction
        // time. Replicating the edge pixels into a small local tile costs
        // (w+7)*(h+7) copies and lets the vector kernel run instead.
        //
        // Bit-identical by construction: the tile holds exactly `refp.px()` for
        // every position the kernel can touch, which is what the scalar path
        // reads. `refp.px` clamps to the last row/column, so this is edge
        // replication, not zero-fill.
        if has_avx2() && w <= 64 && h <= 64 && !no_edge_tile() {
            const TS: usize = 72; // 64 + 7, rounded up
            thread_local! {
                static TILE: std::cell::RefCell<[u16; TS * TS]> =
                    const { std::cell::RefCell::new([0; TS * TS]) };
            }
            let (tw, th) = (w + 7, h + 7);
            return TILE.with(|cell| {
                let mut tile = cell.borrow_mut();
                for ty in 0..th {
                    let sy = by - 3 + ty as i32;
                    let row = ty * TS;
                    for tx in 0..tw {
                        tile[row + tx] = refp.px(bx - 3 + tx as i32, sy) as u16;
                    }
                }
                let tref = RefPlane {
                    buf: &tile[..],
                    stride: TS,
                    w: tw as i32,
                    h: th as i32,
                };
                // SAFETY: AVX2 confirmed; the block sits at (3,3) in a tile that
                // extends 3 left/up and 4 right/down, so every tap is in bounds.
                unsafe {
                    predict_block_avx2(
                        &tref, 3, 3, fx, fy, subx, suby, dst, dst_stride, w, h, max, avg,
                    );
                }
            });
        }
    }

    // NEON fast path (aarch64): same interior-block / single-ref condition.
    #[cfg(target_arch = "aarch64")]
    {
        let (subx, suby) = (subpel_x != 0, subpel_y != 0);
        let (nl, nr) = if subx { (3, 4) } else { (0, 0) };
        let (nt, nb) = if suby { (3, 4) } else { (0, 0) };
        let in_bounds = bx - nl >= 0
            && bx + w as i32 + nr <= refp.w
            && by - nt >= 0
            && by + h as i32 + nb <= refp.h;
        if in_bounds && has_neon() {
            // SAFETY: bounds checked above; NEON is the aarch64 baseline.
            unsafe {
                predict_block_neon(
                    refp, bx, by, fx, fy, subx, suby, dst, dst_stride, w, h, max, avg,
                );
            }
            return;
        }
    }

    let put = |dst: &mut [u16], o: usize, val: u16| {
        dst[o] = if avg {
            round_pow2(dst[o] as i32 + val as i32, 1) as u16
        } else {
            val
        };
    };

    match (subpel_x != 0, subpel_y != 0) {
        (false, false) => {
            for y in 0..h {
                for x in 0..w {
                    let v = refp.px(bx + x as i32, by + y as i32) as u16;
                    put(dst, y * dst_stride + x, v);
                }
            }
        }
        (true, false) => {
            for y in 0..h {
                for x in 0..w {
                    let mut sum = 0i32;
                    for (k, &f) in fx.iter().enumerate() {
                        sum += refp.px(bx + x as i32 + k as i32 - 3, by + y as i32) * f;
                    }
                    put(
                        dst,
                        y * dst_stride + x,
                        clip_pixel(round_pow2(sum, FILTER_BITS), max),
                    );
                }
            }
        }
        (false, true) => {
            for y in 0..h {
                for x in 0..w {
                    let mut sum = 0i32;
                    for (k, &f) in fy.iter().enumerate() {
                        sum += refp.px(bx + x as i32, by + y as i32 + k as i32 - 3) * f;
                    }
                    put(
                        dst,
                        y * dst_stride + x,
                        clip_pixel(round_pow2(sum, FILTER_BITS), max),
                    );
                }
            }
        }
        (true, true) => {
            // Horizontal pass into an intermediate (h + 7 rows), then vertical.
            let tmp_h = h + TAPS - 1;
            MC_TMP.with(|cell| {
                let mut tmp = cell.borrow_mut();
                for r in 0..tmp_h {
                    let sy = by + r as i32 - 3;
                    for x in 0..w {
                        let mut sum = 0i32;
                        for (k, &f) in fx.iter().enumerate() {
                            sum += refp.px(bx + x as i32 + k as i32 - 3, sy) * f;
                        }
                        tmp[r * w + x] = clip_pixel(round_pow2(sum, FILTER_BITS), max);
                    }
                }
                for y in 0..h {
                    for x in 0..w {
                        let mut sum = 0i32;
                        for (k, &f) in fy.iter().enumerate() {
                            sum += tmp[(y + k) * w + x] as i32 * f;
                        }
                        put(
                            dst,
                            y * dst_stride + x,
                            clip_pixel(round_pow2(sum, FILTER_BITS), max),
                        );
                    }
                }
            });
        }
    }
}

/// Scaled motion compensation (libvpx `vpx_scaled_2d_c`): when the reference
/// frame was coded at a different resolution, the source is resampled with a
/// per-output-pixel `x_step_q4`/`y_step_q4` advance (16 = no scaling). Two-pass:
/// an 8-tap horizontal pass into a tall intermediate, then an 8-tap vertical
/// pass. `(bx, by)` is the integer-pel source origin; `subpel_x/y` the starting
/// 1/16-pel phase. Reads are edge-clamped exactly like [`predict_block`].
#[allow(clippy::too_many_arguments)]
pub fn scaled_predict_block(
    refp: &RefPlane,
    bx: i32,
    by: i32,
    subpel_x: usize,
    subpel_y: usize,
    x_step_q4: i32,
    y_step_q4: i32,
    filter: usize,
    dst: &mut [u16],
    dst_stride: usize,
    w: usize,
    h: usize,
    avg: bool,
    max: i32,
) {
    let fil = &SUBPEL_FILTERS[filter];
    // Intermediate height covers every source row the vertical pass can touch.
    let int_h = (((h as i32 - 1) * y_step_q4 + subpel_y as i32) >> 4) as usize + TAPS;
    let mut tmp = vec![0u16; int_h * w];
    // Horizontal pass: intermediate row `r` is source row `by - 3 + r`.
    for r in 0..int_h {
        let sy = by + r as i32 - 3;
        let mut x_q4 = subpel_x as i32;
        for x in 0..w {
            let sx = bx + (x_q4 >> 4);
            let f = &fil[(x_q4 & 15) as usize];
            let mut sum = 0i32;
            for (k, &c) in f.iter().enumerate() {
                sum += refp.px(sx + k as i32 - 3, sy) * c;
            }
            tmp[r * w + x] = clip_pixel(round_pow2(sum, FILTER_BITS), max);
            x_q4 += x_step_q4;
        }
    }
    // Vertical pass over the intermediate.
    let put = |dst: &mut [u16], o: usize, val: u16| {
        dst[o] = if avg {
            round_pow2(dst[o] as i32 + val as i32, 1) as u16
        } else {
            val
        };
    };
    for x in 0..w {
        let mut y_q4 = subpel_y as i32;
        for y in 0..h {
            let row = (y_q4 >> 4) as usize;
            let f = &fil[(y_q4 & 15) as usize];
            let mut sum = 0i32;
            for (k, &c) in f.iter().enumerate() {
                sum += tmp[(row + k) * w + x] as i32 * c;
            }
            put(
                dst,
                y * dst_stride + x,
                clip_pixel(round_pow2(sum, FILTER_BITS), max),
            );
            y_q4 += y_step_q4;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn filter_taps_sum_to_128() {
        // Every sub-pel kernel must sum to 128 (unity gain at FILTER_BITS=7).
        for f in &SUBPEL_FILTERS {
            for phase in f {
                assert_eq!(phase.iter().sum::<i32>(), 128);
            }
        }
        // Phase 0 is the identity tap for all filters.
        for f in &SUBPEL_FILTERS {
            assert_eq!(f[0], [0, 0, 0, 128, 0, 0, 0, 0]);
        }
    }

    /// Bit-exact parity gate for the aarch64 NEON convolution kernel. Runs only
    /// on an aarch64 target; mirrors the scalar `(Σ src·f + 64) >> 7` clamp over
    /// every filter/phase, both passes (tap_stride 1 and a row stride), the <4
    /// SIMD tail, and 8/10/12-bit ranges. This is the gate that validates the
    /// NEON path (which is written to be bit-exact by construction).
    #[cfg(target_arch = "aarch64")]
    #[test]
    fn conv8_neon_matches_scalar() {
        if !has_neon() {
            return;
        }
        let stride = 80usize;
        let mut s = 0x1234_5678u32;
        let mut rng = || {
            s ^= s << 13;
            s ^= s >> 17;
            s ^= s << 5;
            s
        };
        for &max in &[255i32, 1023, 4095] {
            let src: Vec<u16> = (0..stride * 40)
                .map(|_| (rng() % (max as u32 + 1)) as u16)
                .collect();
            for filter in 0..SUBPEL_FILTERS.len() {
                for &phase in &[0usize, 1, 7, 8, 15] {
                    let f = &SUBPEL_FILTERS[filter][phase];
                    for &tap_stride in &[1usize, stride] {
                        for n in [1usize, 3, 4, 7, 8, 13, 16] {
                            let base = 5 * stride + 5;
                            let mut got = vec![0u16; n];
                            unsafe {
                                conv8_neon(
                                    src.as_ptr().add(base),
                                    tap_stride,
                                    f,
                                    got.as_mut_ptr(),
                                    n,
                                    max,
                                    false,
                                );
                            }
                            let want: Vec<u16> = (0..n)
                                .map(|i| {
                                    let mut sum = 0i32;
                                    for k in 0..8 {
                                        sum += src[base + i + k * tap_stride] as i32 * f[k];
                                    }
                                    ((sum + 64) >> 7).clamp(0, max) as u16
                                })
                                .collect();
                            assert_eq!(got, want, "max={max} filter={filter} phase={phase} tap_stride={tap_stride} n={n}");
                        }
                    }
                }
            }
        }
    }

    /// Bit-exact parity gate for the AVX2 convolution kernel (`madd_epi16` path).
    /// Mirrors the scalar `(Σ src·f + 64) >> 7` clamp over every filter/phase, both
    /// passes (tap_stride 1 and a row stride), the `<8` SIMD tail, and 8/10/12-bit.
    #[cfg(target_arch = "x86_64")]
    #[test]
    fn conv8_avx2_matches_scalar() {
        if !has_avx2() {
            return;
        }
        let stride = 80usize;
        let mut s = 0x2468_1357u32;
        let mut rng = || {
            s ^= s << 13;
            s ^= s >> 17;
            s ^= s << 5;
            s
        };
        for &max in &[255i32, 1023, 4095] {
            let src: Vec<u16> = (0..stride * 40)
                .map(|_| (rng() % (max as u32 + 1)) as u16)
                .collect();
            for filter in 0..SUBPEL_FILTERS.len() {
                for phase in 0..16 {
                    let f = &SUBPEL_FILTERS[filter][phase];
                    for &tap_stride in &[1usize, stride] {
                        for n in [1usize, 3, 7, 8, 9, 13, 16, 24] {
                            let base = 5 * stride + 5;
                            for &avg in &[false, true] {
                                let mut got = vec![100u16; n];
                                let mut want = got.clone();
                                unsafe {
                                    conv8_avx2(
                                        src.as_ptr().add(base),
                                        tap_stride,
                                        f,
                                        got.as_mut_ptr(),
                                        n,
                                        max,
                                        avg,
                                    );
                                }
                                for (i, w) in want.iter_mut().enumerate() {
                                    let mut sum = 0i32;
                                    for k in 0..8 {
                                        sum += src[base + i + k * tap_stride] as i32 * f[k];
                                    }
                                    let v = ((sum + 64) >> 7).clamp(0, max);
                                    *w = if avg {
                                        ((v + *w as i32 + 1) >> 1) as u16
                                    } else {
                                        v as u16
                                    };
                                }
                                assert_eq!(got, want, "max={max} filter={filter} phase={phase} ts={tap_stride} n={n} avg={avg}");
                            }
                        }
                    }
                }
            }
        }
    }

    #[test]
    fn full_pel_copy_is_exact() {
        // subpel (0,0) copies the reference block verbatim.
        let w = 8;
        let buf: Vec<u16> = (0..64u16).collect();
        let refp = RefPlane {
            buf: &buf,
            stride: w,
            w: 8,
            h: 8,
        };
        let mut dst = [0u16; 16];
        predict_block(&refp, 1, 1, 0, 0, 0, &mut dst, 4, 4, 4, false, 255);
        for y in 0..4 {
            for x in 0..4 {
                assert_eq!(dst[y * 4 + x], buf[(1 + y) * 8 + (1 + x)]);
            }
        }
    }

    #[test]
    fn horiz_matches_manual_eighttap() {
        // One interior pixel, EIGHTTAP phase 8, computed by the same formula.
        let buf: Vec<u16> = (0..256).map(|i| i as u16).collect(); // 16x16 ramp
        let refp = RefPlane {
            buf: &buf,
            stride: 16,
            w: 16,
            h: 16,
        };
        let mut dst = [0u16; 1];
        predict_block(&refp, 5, 5, 8, 0, 0, &mut dst, 1, 1, 1, false, 255);
        let f = &SUBPEL_FILTERS[0][8];
        let mut sum = 0i32;
        for (k, &c) in f.iter().enumerate() {
            sum += buf[5 * 16 + (5 + k - 3)] as i32 * c;
        }
        assert_eq!(dst[0], clip_pixel(round_pow2(sum, FILTER_BITS), 255));
    }

    #[test]
    fn avg_rounds_toward_existing() {
        let buf = vec![200u16; 64];
        let refp = RefPlane {
            buf: &buf,
            stride: 8,
            w: 8,
            h: 8,
        };
        let mut dst = [100u16; 16];
        predict_block(&refp, 0, 0, 0, 0, 0, &mut dst, 4, 4, 4, true, 255);
        // round((100 + 200)/2) = 150.
        assert!(dst.iter().take(4).all(|&v| v == 150));
    }

    /// Byte-identity gate for the fused 8×8 two-pass kernel vs the SCALAR
    /// reference path, across every filter × subpel-phase pair, random content,
    /// and 8/10/12-bit clamp ranges (interior placement, matching its dispatch).
    #[cfg(target_arch = "x86_64")]
    #[test]
    fn predict8x8_fused_matches_scalar() {
        if !has_avx2() {
            return;
        }
        let mut s = 0x0f0f_5a5a_1234_9999u64;
        let mut xs = move || {
            s ^= s << 13;
            s ^= s >> 7;
            s ^= s << 17;
            s
        };
        let (pw, ph, stride) = (48i32, 48i32, 48usize);
        for &max in &[255i32, 1023, 4095] {
            let buf: Vec<u16> = (0..stride * ph as usize)
                .map(|_| (xs() % (max as u64 + 1)) as u16)
                .collect();
            let refp = RefPlane {
                buf: &buf,
                stride,
                w: pw,
                h: ph,
            };
            for filter in 0..4usize {
                for phase_x in 1..16usize {
                    for phase_y in [1usize, 7, 8, 15] {
                        let (bx, by) = (10 + (xs() % 20) as i32, 10 + (xs() % 20) as i32);
                        let fx = &SUBPEL_FILTERS[filter][phase_x];
                        let fy = &SUBPEL_FILTERS[filter][phase_y];
                        // Scalar oracle: the generic clamped-px path.
                        let mut want = [0u16; 64];
                        for y in 0..8i32 {
                            for x in 0..8i32 {
                                let mut tmp_col = [0i32; 8];
                                for (k, t) in tmp_col.iter_mut().enumerate() {
                                    let mut sum = 0i32;
                                    for (j, &f) in fx.iter().enumerate() {
                                        sum += refp
                                            .px(bx + x + j as i32 - 3, by + y + k as i32 - 3)
                                            * f;
                                    }
                                    *t = clip_pixel(round_pow2(sum, 7), max) as i32;
                                }
                                let mut sum = 0i32;
                                for (k, &f) in fy.iter().enumerate() {
                                    sum += tmp_col[k] * f;
                                }
                                want[(y * 8 + x) as usize] = clip_pixel(round_pow2(sum, 7), max);
                            }
                        }
                        let mut got = [0u16; 64];
                        unsafe {
                            predict8x8_hv_avx2(&refp, bx, by, fx, fy, got.as_mut_ptr(), 8, max);
                        }
                        assert_eq!(
                            got, want,
                            "filter={filter} px={phase_x} py={phase_y} max={max}"
                        );
                    }
                }
            }
        }
    }

    /// Byte-identity gate for the fused single-pass 8×8 kernels (h-only and the
    /// sliding-window v-only) vs the scalar reference, all filters × phases ×
    /// clamp ranges.
    #[cfg(target_arch = "x86_64")]
    #[test]
    fn predict8x8_single_pass_matches_scalar() {
        if !has_avx2() {
            return;
        }
        let mut s = 0x7777_1212_dead_4444u64;
        let mut xs = move || {
            s ^= s << 13;
            s ^= s >> 7;
            s ^= s << 17;
            s
        };
        let (pw, ph, stride) = (48i32, 48i32, 48usize);
        for &max in &[255i32, 1023, 4095] {
            let buf: Vec<u16> = (0..stride * ph as usize)
                .map(|_| (xs() % (max as u64 + 1)) as u16)
                .collect();
            let refp = RefPlane {
                buf: &buf,
                stride,
                w: pw,
                h: ph,
            };
            for filter in 0..4usize {
                for phase in 1..16usize {
                    let (bx, by) = (10 + (xs() % 20) as i32, 10 + (xs() % 20) as i32);
                    let f = &SUBPEL_FILTERS[filter][phase];
                    // Horizontal-only oracle + kernel.
                    let mut want = [0u16; 64];
                    for y in 0..8i32 {
                        for x in 0..8i32 {
                            let mut sum = 0i32;
                            for (k, &t) in f.iter().enumerate() {
                                sum += refp.px(bx + x + k as i32 - 3, by + y) * t;
                            }
                            want[(y * 8 + x) as usize] = clip_pixel(round_pow2(sum, 7), max);
                        }
                    }
                    let mut got = [0u16; 64];
                    unsafe { predict8x8_h_avx2(&refp, bx, by, f, got.as_mut_ptr(), 8, max) };
                    assert_eq!(got, want, "H filter={filter} phase={phase} max={max}");
                    // Vertical-only oracle + kernel.
                    for y in 0..8i32 {
                        for x in 0..8i32 {
                            let mut sum = 0i32;
                            for (k, &t) in f.iter().enumerate() {
                                sum += refp.px(bx + x, by + y + k as i32 - 3) * t;
                            }
                            want[(y * 8 + x) as usize] = clip_pixel(round_pow2(sum, 7), max);
                        }
                    }
                    unsafe { predict8x8_v_avx2(&refp, bx, by, f, got.as_mut_ptr(), 8, max) };
                    assert_eq!(got, want, "V filter={filter} phase={phase} max={max}");
                }
            }
        }
    }

    /// Pins the saturation preconditions the 16-wide pmaddubsw kernels rely on
    /// (see the proof comment in `u8score`). If a filter table ever changes and
    /// breaks one of these, this test fails BEFORE the kernels can go wrong.
    #[test]
    fn pmaddubsw_preconditions_hold() {
        for f in &SUBPEL_FILTERS {
            for phase in 1..16 {
                let t = &f[phase];
                let pos = |v: i32| if v > 0 { v } else { 0 };
                for x in t {
                    assert!(x.abs() <= 127, "tap {x} does not fit i8");
                }
                for k in 0..4 {
                    let pair_pos = pos(t[2 * k]) + pos(t[2 * k + 1]);
                    assert!(255 * pair_pos <= 32767, "pair {k} saturates pmaddubsw");
                }
                let g1 = pos(t[0]) + pos(t[1]) + pos(t[4]) + pos(t[5]);
                let g2 = pos(t[2]) + pos(t[3]) + pos(t[6]) + pos(t[7]);
                assert!(255 * g1 <= 32767, "group (01+45) saturates");
                assert!(255 * g2 <= 32767, "group (23+67) saturates");
                let neg: i32 = t.iter().filter(|&&v| v < 0).sum();
                assert!(255 * neg >= -32768, "negative reach saturates");
            }
        }
    }

    /// Adversarial saturation gate: reference content crafted (255 under the
    /// positive taps, 0 under the negatives per filter symmetry is approximated
    /// by alternating extremes) so the final saturating adds actually FIRE; the
    /// u8 kernels must still match the exact u16 path bit-for-bit (the
    /// harmless-by-clamp convergence).
    #[cfg(target_arch = "x86_64")]
    #[test]
    fn u8_score_saturation_adversarial() {
        if !has_avx2() {
            return;
        }
        let (pw, ph, stride) = (48i32, 48i32, 48usize);
        // Extreme patterns: checkerboards and stripes of 0/255 at several pitches
        // maximize |Σ s·f| through the two-pass pipeline.
        for pattern in 0..6u32 {
            let refbuf16: Vec<u16> = (0..stride * ph as usize)
                .map(|i| {
                    let (x, y) = (i % stride, i / stride);
                    let bit = match pattern {
                        0 => x % 2 == 0,
                        1 => y % 2 == 0,
                        2 => (x + y) % 2 == 0,
                        3 => x % 4 < 2,
                        4 => y % 4 < 2,
                        _ => (x / 2 + y / 2) % 2 == 0,
                    };
                    if bit {
                        255
                    } else {
                        0
                    }
                })
                .collect();
            let refbuf8: Vec<u8> = refbuf16.iter().map(|&v| v as u8).collect();
            let srcbuf8: Vec<u8> = vec![128u8; stride * ph as usize];
            let srcbuf16: Vec<u16> = srcbuf8.iter().map(|&v| v as u16).collect();
            let refp16 = RefPlane {
                buf: &refbuf16,
                stride,
                w: pw,
                h: ph,
            };
            let refp8 = RefPlane8 {
                buf: &refbuf8,
                stride,
                w: pw,
                h: ph,
            };
            for filter in 0..4usize {
                for px in 0..16usize {
                    for py in 0..16usize {
                        let (bx, by) = (16, 16);
                        let mut pred = [0u16; 64];
                        predict_block(
                            &refp16, bx, by, px, py, filter, &mut pred, 8, 8, 8, false, 255,
                        );
                        let mut want = 0u32;
                        for y in 0..8usize {
                            for x in 0..8usize {
                                let sv = srcbuf16[(20 + y) * stride + 20 + x] as i32;
                                want += (sv - pred[y * 8 + x] as i32).unsigned_abs();
                            }
                        }
                        let got = unsafe {
                            subpel_score8x8_u8(
                                &refp8,
                                bx,
                                by,
                                px,
                                py,
                                filter,
                                srcbuf8.as_ptr().add(20 * stride + 20),
                                stride,
                            )
                        };
                        assert_eq!(
                            got, want,
                            "pattern={pattern} filter={filter} px={px} py={py}"
                        );
                    }
                }
            }
        }
    }

    /// The SIMD bilinear scorer must equal its scalar oracle for every phase
    /// pair on random and extreme content (convexity means no clamp paths, but
    /// the rounding must match exactly).
    #[cfg(target_arch = "x86_64")]
    #[test]
    fn bilinear_score_matches_scalar() {
        if !has_avx2() {
            return;
        }
        let mut st = 0xb111_2026_0716_ccccu64;
        let mut xs = move || {
            st ^= st << 13;
            st ^= st >> 7;
            st ^= st << 17;
            st
        };
        let (pw, ph, stride) = (48i32, 48i32, 48usize);
        for mode in 0..2u32 {
            let refbuf8: Vec<u8> = (0..stride * ph as usize)
                .map(|i| match mode {
                    0 => (xs() % 256) as u8,
                    _ => {
                        let (x, y) = (i % stride, i / stride);
                        if (x + y) % 2 == 0 {
                            255
                        } else {
                            0
                        }
                    }
                })
                .collect();
            let srcbuf8: Vec<u8> = (0..stride * ph as usize)
                .map(|_| (xs() % 256) as u8)
                .collect();
            let refp8 = RefPlane8 {
                buf: &refbuf8,
                stride,
                w: pw,
                h: ph,
            };
            for px in 0..16usize {
                for py in 0..16usize {
                    let (bx, by) = (4 + (xs() % 30) as usize, 4 + (xs() % 30) as usize);
                    let (sx, sy) = (4 + (xs() % 30) as usize, 4 + (xs() % 30) as usize);
                    let want = bilinear_score8x8_scalar(
                        &refbuf8,
                        stride,
                        bx,
                        by,
                        px,
                        py,
                        &srcbuf8,
                        sy * stride + sx,
                        stride,
                    );
                    let got = unsafe {
                        subpel_bilinear_score8x8_u8(
                            &refp8,
                            bx as i32,
                            by as i32,
                            px,
                            py,
                            srcbuf8.as_ptr().add(sy * stride + sx),
                            stride,
                        )
                    };
                    assert_eq!(got, want, "mode={mode} px={px} py={py} bx={bx} by={by}");
                }
            }
        }
    }

    /// The u8 fused SSE kernels must equal the u16 path (predict_block + exact
    /// Σ(s−p)²) for every filter × phase pair, on random AND adversarial
    /// (0/255 pattern) content.
    #[cfg(target_arch = "x86_64")]
    #[test]
    fn u8_sse_matches_u16_path() {
        if !has_avx2() {
            return;
        }
        let mut st = 0x5ee5_0716_2026_bbbbu64;
        let mut xs = move || {
            st ^= st << 13;
            st ^= st >> 7;
            st ^= st << 17;
            st
        };
        let (pw, ph, stride) = (48i32, 48i32, 48usize);
        for mode in 0..3u32 {
            let refbuf16: Vec<u16> = (0..stride * ph as usize)
                .map(|i| match mode {
                    0 => (xs() % 256) as u16,
                    1 => {
                        let (x, y) = (i % stride, i / stride);
                        if (x + y) % 2 == 0 {
                            255
                        } else {
                            0
                        }
                    }
                    _ => {
                        if (i % stride) % 4 < 2 {
                            255
                        } else {
                            0
                        }
                    }
                })
                .collect();
            let refbuf8: Vec<u8> = refbuf16.iter().map(|&v| v as u8).collect();
            let srcbuf16: Vec<u16> = (0..stride * ph as usize)
                .map(|_| (xs() % 256) as u16)
                .collect();
            let srcbuf8: Vec<u8> = srcbuf16.iter().map(|&v| v as u8).collect();
            let refp16 = RefPlane {
                buf: &refbuf16,
                stride,
                w: pw,
                h: ph,
            };
            let refp8 = RefPlane8 {
                buf: &refbuf8,
                stride,
                w: pw,
                h: ph,
            };
            for filter in 0..4usize {
                for px in 0..16usize {
                    for py in [0usize, 1, 5, 8, 12, 15] {
                        let (bx, by) = (10 + (xs() % 18) as i32, 10 + (xs() % 18) as i32);
                        let (sx, sy) = (8 + (xs() % 18) as usize, 8 + (xs() % 18) as usize);
                        let mut pred = [0u16; 64];
                        predict_block(
                            &refp16, bx, by, px, py, filter, &mut pred, 8, 8, 8, false, 255,
                        );
                        let mut want = 0u32;
                        for y in 0..8usize {
                            for x in 0..8usize {
                                let d = srcbuf16[(sy + y) * stride + sx + x] as i32
                                    - pred[y * 8 + x] as i32;
                                want += (d * d) as u32;
                            }
                        }
                        let got = unsafe {
                            subpel_sse8x8_u8(
                                &refp8,
                                bx,
                                by,
                                px,
                                py,
                                filter,
                                srcbuf8.as_ptr().add(sy * stride + sx),
                                stride,
                            )
                        };
                        assert_eq!(got, want, "mode={mode} filter={filter} px={px} py={py}");
                    }
                }
            }
        }
    }

    /// The u8 fused score kernels must equal the u16 path (scalar predict_block
    /// + SAD) exactly, for every filter x (subpel_x, subpel_y) phase pair on
    /// 8-bit content — the proof that scoring in the u8 mirror is bit-identical.
    #[cfg(target_arch = "x86_64")]
    #[test]
    fn u8_score_matches_u16_path() {
        if !has_avx2() {
            return;
        }
        let mut s = 0x0dd0_2026_0715_aaaau64;
        let mut xs = move || {
            s ^= s << 13;
            s ^= s >> 7;
            s ^= s << 17;
            s
        };
        let (pw, ph, stride) = (48i32, 48i32, 48usize);
        let refbuf16: Vec<u16> = (0..stride * ph as usize)
            .map(|_| (xs() % 256) as u16)
            .collect();
        let refbuf8: Vec<u8> = refbuf16.iter().map(|&v| v as u8).collect();
        let srcbuf16: Vec<u16> = (0..stride * ph as usize)
            .map(|_| (xs() % 256) as u16)
            .collect();
        let srcbuf8: Vec<u8> = srcbuf16.iter().map(|&v| v as u8).collect();
        let refp16 = RefPlane {
            buf: &refbuf16,
            stride,
            w: pw,
            h: ph,
        };
        let refp8 = RefPlane8 {
            buf: &refbuf8,
            stride,
            w: pw,
            h: ph,
        };
        for filter in 0..4usize {
            for px in 0..16usize {
                for py in [0usize, 1, 7, 8, 15] {
                    let (bx, by) = (10 + (xs() % 20) as i32, 10 + (xs() % 20) as i32);
                    let (sx, sy) = (8 + (xs() % 20) as usize, 8 + (xs() % 20) as usize);
                    // u16 oracle: full predict + scalar SAD.
                    let mut pred = [0u16; 64];
                    predict_block(
                        &refp16, bx, by, px, py, filter, &mut pred, 8, 8, 8, false, 255,
                    );
                    let mut want = 0u32;
                    for y in 0..8usize {
                        for x in 0..8usize {
                            let sv = srcbuf16[(sy + y) * stride + sx + x] as i32;
                            want += (sv - pred[y * 8 + x] as i32).unsigned_abs();
                        }
                    }
                    let got = unsafe {
                        subpel_score8x8_u8(
                            &refp8,
                            bx,
                            by,
                            px,
                            py,
                            filter,
                            srcbuf8.as_ptr().add(sy * stride + sx),
                            stride,
                        )
                    };
                    assert_eq!(got, want, "filter={filter} px={px} py={py} bx={bx} by={by}");
                }
            }
        }
    }

    #[test]
    fn compound_avg_matches_mc_blend() {
        // Exercises the compound (avg) kernel — the AVX2 8-wide blend for w≥8 and
        // the scalar tail for w=4 — across all four subpel cases. `avg=true` must
        // equal `(mc + dst0 + 1) >> 1` where `mc` is the same block with avg=false.
        let stride = 48usize;
        let buf: Vec<u16> = (0..stride * 48)
            .map(|i| ((i * 7 + 13) % 256) as u16)
            .collect();
        let refp = RefPlane {
            buf: &buf,
            stride,
            w: 48,
            h: 48,
        };
        let max = 255;
        for &(sx, sy) in &[(0usize, 0usize), (8, 0), (0, 8), (8, 8), (4, 12)] {
            for &w in &[4usize, 8, 16] {
                let h = w;
                let (bx, by) = (12i32, 12i32); // window stays interior → AVX2 path
                let dst0: Vec<u16> = (0..w * h).map(|i| ((i * 3 + 41) % 256) as u16).collect();

                let mut dst_avg = dst0.clone();
                predict_block(&refp, bx, by, sx, sy, 0, &mut dst_avg, w, w, h, true, max);

                let mut mc = vec![0u16; w * h];
                predict_block(&refp, bx, by, sx, sy, 0, &mut mc, w, w, h, false, max);
                let manual: Vec<u16> = dst0
                    .iter()
                    .zip(&mc)
                    .map(|(&a, &b)| ((a as i32 + b as i32 + 1) >> 1) as u16)
                    .collect();

                assert_eq!(
                    dst_avg, manual,
                    "compound mismatch: subpel ({sx},{sy}), w={w}"
                );
            }
        }
    }
}

#[cfg(test)]
mod mc_microbench {
    //! In-process microbenchmark for inter prediction — the same instrument
    //! discipline as `transform::tx_microbench`, for the same reason: the decode
    //! stage wall carries ±7..14% noise here, far more than most MC bricks.
    //!
    //!   cargo test -p rusty_vp9 --release mc_microbench -- --ignored --nocapture
    use super::*;

    fn rdtsc() -> u64 {
        #[cfg(target_arch = "x86_64")]
        // SAFETY: `_rdtsc` has no operands and no side effects.
        unsafe {
            std::arch::x86_64::_rdtsc()
        }
        #[cfg(not(target_arch = "x86_64"))]
        0
    }

    /// Times `predict_block` alone, with every parameter hoisted out of the
    /// timed region — so this is the KERNEL cost, excluding `mc_one`'s per-call
    /// setup (Arc clone, RefPlane build, MV clamp, scaled check).
    fn bench(w: usize, h: usize, subx: usize, suby: usize, interior: bool) -> f64 {
        let (rw, rh) = (352usize, 288usize);
        let stride = 448usize;
        let mut src = vec![0u16; stride * (rh + 80)];
        let mut st = 0x1234_5678_9ABC_DEF0u64;
        for p in src.iter_mut() {
            st ^= st << 13;
            st ^= st >> 7;
            st ^= st << 17;
            *p = (st % 256) as u16;
        }
        let refp = RefPlane {
            buf: &src,
            stride,
            w: rw as i32,
            h: rh as i32,
        };
        let mut dst = vec![128u16; stride * (h + 8)];
        // `interior` picks a position where the AVX2 in-bounds test passes;
        // otherwise a left-edge position that forces the scalar clamp path.
        let (bx, by) = if interior {
            (40i32, 40i32)
        } else {
            (0i32, 0i32)
        };
        let iters = 2000usize;
        let mut best = f64::MAX;
        for _ in 0..9 {
            let t0 = rdtsc();
            for _ in 0..iters {
                predict_block(
                    &refp, bx, by, subx, suby, 0, &mut dst, stride, w, h, false, 255,
                );
            }
            best = best.min((rdtsc() - t0) as f64 / iters as f64);
        }
        best
    }

    #[test]
    #[ignore = "microbenchmark; run explicitly with --ignored --nocapture"]
    fn profile_predict_block() {
        let _ = bench(8, 8, 4, 4, true); // warm up
        println!("\npredict_block — best-of-9, cycles/call");
        println!(
            "  {:<10} {:>10} {:>10} {:>10} {:>12}",
            "size", "full-pel", "x-only", "xy", "xy(edge)"
        );
        for (w, h) in [
            (4usize, 4usize),
            (4, 8),
            (8, 8),
            (8, 16),
            (16, 16),
            (32, 32),
            (64, 64),
        ] {
            println!(
                "  {:<10} {:>10.1} {:>10.1} {:>10.1} {:>12.1}",
                format!("{w}x{h}"),
                bench(w, h, 0, 0, true),
                bench(w, h, 4, 0, true),
                bench(w, h, 4, 4, true),
                bench(w, h, 4, 4, false),
            );
        }
    }
}
