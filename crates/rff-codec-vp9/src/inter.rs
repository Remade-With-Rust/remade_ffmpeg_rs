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
unsafe fn conv8_avx2(src: *const u16, tap_stride: usize, f: &[i32; 8], dst: *mut u16, n: usize, max: i32) {
    use std::arch::x86_64::*;
    let round = _mm256_set1_epi32(64);
    let maxv = _mm256_set1_epi32(max);
    let zero = _mm256_setzero_si256();
    let fk = [
        _mm256_set1_epi32(f[0]), _mm256_set1_epi32(f[1]), _mm256_set1_epi32(f[2]), _mm256_set1_epi32(f[3]),
        _mm256_set1_epi32(f[4]), _mm256_set1_epi32(f[5]), _mm256_set1_epi32(f[6]), _mm256_set1_epi32(f[7]),
    ];
    let mut i = 0usize;
    while i + 8 <= n {
        let mut acc = zero;
        for k in 0..8 {
            let s = _mm256_cvtepu16_epi32(_mm_loadu_si128(src.add(i + k * tap_stride) as *const __m128i));
            acc = _mm256_add_epi32(acc, _mm256_mullo_epi32(s, fk[k]));
        }
        acc = _mm256_srai_epi32::<7>(_mm256_add_epi32(acc, round));
        acc = _mm256_min_epi32(_mm256_max_epi32(acc, zero), maxv);
        // pack i32x8 -> u16x8: packus gives [a0..3|a0..3 || a4..7|a4..7]; pull 64-bit lanes 0,2.
        let packed = _mm256_packus_epi32(acc, acc);
        let perm = _mm256_permute4x64_epi64::<0x08>(packed);
        _mm_storeu_si128(dst.add(i) as *mut __m128i, _mm256_castsi256_si128(perm));
        i += 8;
    }
    while i < n {
        let mut sum = 0i32;
        for k in 0..8 {
            sum += *src.add(i + k * tap_stride) as i32 * f[k];
        }
        *dst.add(i) = ((sum + 64) >> 7).clamp(0, max) as u16;
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
) {
    let buf = refp.buf.as_ptr();
    let stride = refp.stride;
    let dptr = dst.as_mut_ptr();
    match (subx, suby) {
        (false, false) => {
            for y in 0..h {
                let s = buf.add((by as usize + y) * stride + bx as usize);
                std::ptr::copy_nonoverlapping(s, dptr.add(y * dst_stride), w);
            }
        }
        (true, false) => {
            for y in 0..h {
                let s = buf.add((by as usize + y) * stride + (bx - 3) as usize);
                conv8_avx2(s, 1, fx, dptr.add(y * dst_stride), w, max);
            }
        }
        (false, true) => {
            for y in 0..h {
                let s = buf.add((by + y as i32 - 3) as usize * stride + bx as usize);
                conv8_avx2(s, stride, fy, dptr.add(y * dst_stride), w, max);
            }
        }
        (true, true) => {
            MC_TMP.with(|cell| {
                let mut tmp = cell.borrow_mut();
                let tmp_h = h + TAPS - 1;
                let tptr = tmp.as_mut_ptr();
                for r in 0..tmp_h {
                    let s = buf.add((by + r as i32 - 3) as usize * stride + (bx - 3) as usize);
                    conv8_avx2(s, 1, fx, tptr.add(r * w), w, max);
                }
                for y in 0..h {
                    conv8_avx2(tptr.add(y * w) as *const u16, w, fy, dptr.add(y * dst_stride), w, max);
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
        if !avg && in_bounds && has_avx2() {
            // SAFETY: bounds checked above; AVX2 confirmed present.
            unsafe {
                predict_block_avx2(refp, bx, by, fx, fy, subx, suby, dst, dst_stride, w, h, max);
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
                    put(dst, y * dst_stride + x, clip_pixel(round_pow2(sum, FILTER_BITS), max));
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
                    put(dst, y * dst_stride + x, clip_pixel(round_pow2(sum, FILTER_BITS), max));
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
                    put(dst, y * dst_stride + x, clip_pixel(round_pow2(sum, FILTER_BITS), max));
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
        dst[o] = if avg { round_pow2(dst[o] as i32 + val as i32, 1) as u16 } else { val };
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
            put(dst, y * dst_stride + x, clip_pixel(round_pow2(sum, FILTER_BITS), max));
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

    #[test]
    fn full_pel_copy_is_exact() {
        // subpel (0,0) copies the reference block verbatim.
        let w = 8;
        let buf: Vec<u16> = (0..64u16).collect();
        let refp = RefPlane { buf: &buf, stride: w, w: 8, h: 8 };
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
        let refp = RefPlane { buf: &buf, stride: 16, w: 16, h: 16 };
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
        let refp = RefPlane { buf: &buf, stride: 8, w: 8, h: 8 };
        let mut dst = [100u16; 16];
        predict_block(&refp, 0, 0, 0, 0, 0, &mut dst, 4, 4, 4, true, 255);
        // round((100 + 200)/2) = 150.
        assert!(dst.iter().take(4).all(|&v| v == 150));
    }
}
