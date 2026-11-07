//! Inverse-transform kernels (§8.6.4).
//!
//! The transform was the decoder's largest stage (27.1% before the scalar
//! rewrite) and the last one with no kernel at all.
//!
//! # What this kernel is
//!
//! One primitive, called from the butterfly's two accumulation sites:
//!
//! ```text
//!   out[j] = SUM over k of  src[k * s_in] * tab[k * tstep][j],  j < LEN
//! ```
//!
//! -- the coefficient-outer form the scalar code was already restructured into.
//! That restructuring is what makes a kernel possible: written output-outer, the
//! inner loop walks the table DOWN a column at a stride of 32 `i16`, which no
//! vector unit wants. Written coefficient-outer it is a contiguous row scan.
//!
//! # Why the compiler would not do it
//!
//! LLVM was given the same loop and declined -- the scalar `idct_sums` emits
//! seven vector instructions, which is incidental register traffic, not a
//! vectorised loop. Three reasons, and the kernel answers each:
//!
//! * **The trip count is 2, 4, 8 or 16**, usually below the threshold LLVM will
//!   widen for. Here `LEN` is a const generic, so the shape is fixed at compile
//!   time and there is no threshold to clear.
//! * **The table is `i16` and the accumulator `i32`**, so every lane needs a
//!   widening the compiler must prove worthwhile. `vpmovsxwd` does it as one
//!   instruction with the load folded in as a memory operand.
//! * **The accumulator spills.** Sixteen `i32` live across the coefficient loop
//!   is sixteen registers; the scalar form keeps them on the stack and reloads
//!   and restores them for every coefficient. Two `ymm` registers hold the same
//!   sixteen values, so the accumulator never leaves the register file.
//!
//! `i32` lanes are what the preceding scalar pass bought: with the `i64`
//! accumulators this code had until today, a 256-bit register would hold four
//! lanes instead of eight.

#[cfg(all(feature = "simd", target_arch = "x86_64"))]
use std::arch::x86_64::*;

use crate::census;

/// Coefficient range after scaling and between the two transform passes
/// (§8.6.4.2: `coeffMin`/`coeffMax` at `extended_precision_processing_flag = 0`).
pub const COEFF_MIN: i32 = -32768;
pub const COEFF_MAX: i32 = 32767;

/// The 4-point DCT (`DCT32[8k][0..4]`), the 4x4 DST (§8.6.4.2) and the
/// 8-point DCT (`DCT32[4k][0..8]`), as `i32` for the broadcast kernels.
/// Pinned to the decoder's `DCT32` by `small_transform_tables_match_dct32`.
pub const DCT4: [[i32; 4]; 4] = [[64, 64, 64, 64], [83, 36, -36, -83], [64, -64, -64, 64], [36, -83, 83, -36]];
pub const DST4: [[i32; 4]; 4] = [[29, 55, 74, 84], [74, 74, 0, -74], [84, -29, -74, 55], [55, -84, 74, -29]];
pub const DCT8: [[i32; 8]; 8] = [
    [64, 64, 64, 64, 64, 64, 64, 64],
    [89, 75, 50, 18, -18, -50, -75, -89],
    [83, 36, -36, -83, -83, -36, 36, 83],
    [75, -18, -89, -50, 50, 89, 18, -75],
    [64, -64, -64, 64, 64, -64, -64, 64],
    [50, -89, 18, 75, -75, -18, 89, -50],
    [36, -83, 83, -36, -36, 83, -83, 36],
    [18, -50, 75, -89, 89, -75, 50, -18],
];

/// Both passes of an `n x n` inverse DCT, or the 4x4 DST when `dst`, in one
/// kernel call. Returns `false` when no kernel applies, and the caller runs
/// its scalar twin -- the oracle, kept in `rusty_h265::itx`.
///
/// `tab` is the flat 32x32 DCT table and `dst_tab` the flat padded 4x32 DST
/// table; the kernel reads `tab[k * tstep * 32 + j]` exactly as `accum` does.
#[allow(clippy::too_many_arguments)]
pub fn idct_block(d: &mut [i32], tmp: &mut [i32], n: usize, nz_w: usize, nz_h: usize, bd_shift: u32, dst: bool, tab: &[i16], dst_tab: &[i16]) -> bool {
    #[cfg(all(feature = "simd", target_arch = "x86_64"))]
    {
        let ok = matches!(n, 4 | 8 | 16 | 32)
            && d.len() >= n * n
            && tmp.len() >= n * n
            && (1..=n).contains(&nz_w)
            && (1..=n).contains(&nz_h)
            && (1..32).contains(&bd_shift)
            && tab.len() >= 32 * 32
            && dst_tab.len() >= 4 * 32
            && (!dst || n == 4);
        if ok && kernel_isa() >= crate::Isa::Avx2 {
            if census::ALWAYS {
                census::bump(&census::ITX_FUSED_SIMD, 1);
            }
            // SAFETY: `ok` proves every load and store: both buffers hold the
            // block, the live counts are inside it, and the tables are the
            // full 32-row DCT and 4-row padded DST.
            unsafe {
                let (dp, tp, t, dt) = (d.as_mut_ptr(), tmp.as_mut_ptr(), tab.as_ptr(), dst_tab.as_ptr());
                match (n, dst) {
                    // The two small sizes take the broadcast-row kernels; the
                    // butterfly block kernel below serves 16 and 32.
                    (4, true) => x86::idct4_avx2::<true>(dp, bd_shift),
                    (4, false) => x86::idct4_avx2::<false>(dp, bd_shift),
                    (8, _) => x86::idct8_avx2(dp, nz_w, nz_h, bd_shift, &DCT8),
                    (16, _) => x86::idct_block_avx2::<16, false>(dp, tp, nz_w, nz_h, bd_shift, t, dt),
                    _ => x86::idct_block_avx2::<32, false>(dp, tp, nz_w, nz_h, bd_shift, t, dt),
                }
            }
            return true;
        }
    }
    let _ = (d, tmp, n, nz_w, nz_h, bd_shift, dst, tab, dst_tab);
    false
}

/// Scalar twin -- the oracle, and the path on any non-AVX2 machine.
///
/// `tab` is a flat table of 32-wide `i16` rows; coefficient `k` uses row
/// `k * tstep`.
pub fn accum_scalar(out: &mut [i32], src: &[i32], s_in: usize, tab: &[i16], tstep: usize, k0: usize, kstep: usize, nz: usize, len: usize) {
    out[..len].fill(0);
    let mut k = k0;
    while k < nz {
        let c = src[k * s_in];
        if c != 0 {
            let row = &tab[k * tstep * 32..];
            for j in 0..len {
                out[j] += c * row[j] as i32;
            }
        }
        k += kstep;
    }
}

#[cfg(all(feature = "simd", target_arch = "x86_64"))]
mod x86 {
    use super::*;

    /// # Safety
    /// `out` holds `LEN` writable `i32`; every `src[k * s_in]` for
    /// `k = k0, k0+kstep, .. < nz` is readable; each `tab[k * tstep * 32 ..]`
    /// has `LEN` readable `i16`. `LEN` is 4, 8 or 16.
    #[target_feature(enable = "avx2")]
    pub unsafe fn accum_avx2<const LEN: usize>(out: *mut i32, src: *const i32, s_in: usize, tab: *const i16, tstep: usize, k0: usize, kstep: usize, nz: usize) {
        // The accumulator lives here, in registers, for the whole coefficient
        // loop -- the single largest difference from the scalar form.
        let mut a0 = _mm256_setzero_si256();
        let mut a1 = _mm256_setzero_si256();
        let mut s0 = _mm_setzero_si128();

        let mut k = k0;
        while k < nz {
            let c = unsafe { *src.add(k * s_in) };
            // One test per COEFFICIENT skips a whole table row. Sparse blocks
            // are the common case -- the anatomy measured 2.85% of coefficient
            // area live -- so this branch is well predicted and load-bearing.
            if c != 0 {
                let row = unsafe { tab.add(k * tstep * 32) };
                if LEN >= 8 {
                    // The coefficient broadcast is written ONCE and reused, as
                    // `accum_butterfly_avx2` already does. Spelled inline at
                    // both uses it relies on the compiler to common them up,
                    // and the two kernels then read differently for no reason.
                    let cv = _mm256_set1_epi32(c);
                    // `vpmovsxwd` widens eight `i16` to eight `i32` in one
                    // instruction, taking its load as a memory operand.
                    let t0 = _mm256_cvtepi16_epi32(unsafe { _mm_loadu_si128(row as *const __m128i) });
                    a0 = _mm256_add_epi32(a0, _mm256_mullo_epi32(t0, cv));
                    if LEN >= 16 {
                        let t1 = _mm256_cvtepi16_epi32(unsafe { _mm_loadu_si128(row.add(8) as *const __m128i) });
                        a1 = _mm256_add_epi32(a1, _mm256_mullo_epi32(t1, cv));
                    }
                } else {
                    // LEN == 4: a half register is the whole row.
                    let t = _mm_cvtepi16_epi32(unsafe { _mm_loadl_epi64(row as *const __m128i) });
                    s0 = _mm_add_epi32(s0, _mm_mullo_epi32(t, _mm_set1_epi32(c)));
                }
            }
            k += kstep;
        }

        if LEN >= 8 {
            unsafe { _mm256_storeu_si256(out as *mut __m256i, a0) };
            if LEN >= 16 {
                unsafe { _mm256_storeu_si256(out.add(8) as *mut __m256i, a1) };
            }
        } else {
            unsafe { _mm_storeu_si128(out as *mut __m128i, s0) };
        }
    }

    /// Accumulate the odd part AND apply the butterfly output stage, without
    /// the round trip between them.
    ///
    /// Split across two kernels the odd sums are stored to a scratch array by
    /// one and immediately reloaded by the other -- `HALF` stores and `HALF`
    /// loads, 1.66 million times a clip, for values that were already in
    /// registers. Fused, they never leave. The high half is written in REVERSE,
    /// which `vpermd` does in one instruction so the store stays contiguous.
    ///
    /// # Safety
    /// `out` holds `2 * HALF` writable `i32`; `src[k * s_in]` is readable for
    /// every odd `k < nz`; each `tab[k * tstep * 32 ..]` has `HALF` readable
    /// `i16`. `HALF` is 4, 8 or 16.
    #[target_feature(enable = "avx2")]
    pub unsafe fn accum_butterfly_avx2<const HALF: usize>(out: *mut i32, src: *const i32, s_in: usize, tab: *const i16, tstep: usize, nz: usize) {
        let mut a0 = _mm256_setzero_si256();
        let mut a1 = _mm256_setzero_si256();
        let mut s0 = _mm_setzero_si128();
        let mut k = 1;
        while k < nz {
            let c = unsafe { *src.add(k * s_in) };
            if c != 0 {
                let row = unsafe { tab.add(k * tstep * 32) };
                let cv = _mm256_set1_epi32(c);
                if HALF >= 8 {
                    let t0 = _mm256_cvtepi16_epi32(unsafe { _mm_loadu_si128(row as *const __m128i) });
                    a0 = _mm256_add_epi32(a0, _mm256_mullo_epi32(t0, cv));
                    if HALF >= 16 {
                        let t1 = _mm256_cvtepi16_epi32(unsafe { _mm_loadu_si128(row.add(8) as *const __m128i) });
                        a1 = _mm256_add_epi32(a1, _mm256_mullo_epi32(t1, cv));
                    }
                } else {
                    let t = _mm_cvtepi16_epi32(unsafe { _mm_loadl_epi64(row as *const __m128i) });
                    s0 = _mm_add_epi32(s0, _mm_mullo_epi32(t, _mm_set1_epi32(c)));
                }
            }
            k += 2;
        }

        let n = 2 * HALF;
        let rev = _mm256_setr_epi32(7, 6, 5, 4, 3, 2, 1, 0);
        if HALF >= 8 {
            let e0 = unsafe { _mm256_loadu_si256(out as *const __m256i) };
            unsafe { _mm256_storeu_si256(out as *mut __m256i, _mm256_add_epi32(e0, a0)) };
            let d0 = _mm256_permutevar8x32_epi32(_mm256_sub_epi32(e0, a0), rev);
            unsafe { _mm256_storeu_si256(out.add(n - 8) as *mut __m256i, d0) };
            if HALF >= 16 {
                let e1 = unsafe { _mm256_loadu_si256(out.add(8) as *const __m256i) };
                unsafe { _mm256_storeu_si256(out.add(8) as *mut __m256i, _mm256_add_epi32(e1, a1)) };
                let d1 = _mm256_permutevar8x32_epi32(_mm256_sub_epi32(e1, a1), rev);
                unsafe { _mm256_storeu_si256(out.add(n - 16) as *mut __m256i, d1) };
            }
        } else {
            let e = unsafe { _mm_loadu_si128(out as *const __m128i) };
            unsafe { _mm_storeu_si128(out as *mut __m128i, _mm_add_epi32(e, s0)) };
            let d = _mm_shuffle_epi32(_mm_sub_epi32(e, s0), 0x1b);
            unsafe { _mm_storeu_si128(out.add(n - 4) as *mut __m128i, d) };
        }
    }

    /// # Safety
    /// Both pointers hold `n` elements and `n` is a positive multiple of 8.
    #[target_feature(enable = "avx2")]
    pub unsafe fn shift_clip_avx2<const CLIP: bool>(dst: *mut i32, src: *const i32, n: usize, shift: u32, lo: i32, hi: i32) {
        let addv = _mm256_set1_epi32(1i32 << (shift - 1));
        let sh = _mm_cvtsi32_si128(shift as i32);
        let lov = _mm256_set1_epi32(lo);
        let hiv = _mm256_set1_epi32(hi);
        // Two vectors per trip. The body is three or five instructions and the
        // loop's own bookkeeping two, so one vector per trip spent a third of
        // the loop on the counter. `n` is a transform dimension -- 4, 8, 16 or
        // 32 -- so the pair covers everything from 16 up and the single-vector
        // arm below finishes 8 and any odd remainder.
        let one = |i: usize| {
            let v = unsafe { _mm256_loadu_si256(src.add(i) as *const __m256i) };
            let mut r = _mm256_sra_epi32(_mm256_add_epi32(v, addv), sh);
            if CLIP {
                r = _mm256_min_epi32(_mm256_max_epi32(r, lov), hiv);
            }
            unsafe { _mm256_storeu_si256(dst.add(i) as *mut __m256i, r) };
        };
        let mut i = 0;
        while i + 16 <= n {
            one(i);
            one(i + 8);
            i += 16;
        }
        while i + 8 <= n {
            one(i);
            i += 8;
        }
    }

    /// The flattened partial butterfly for ONE `N`-point 1-D transform, with
    /// `nzs` precomputed by the caller. Same calls, same order and same
    /// arithmetic as `rusty_h265::itx::idct_sums`, so the result is
    /// bit-identical to it; what differs is that this is inlined into a
    /// `#[target_feature]` caller that holds the whole block, so the per-call
    /// dispatch, bounds proof and `OnceLock` read of the four `accum` /
    /// `accum_butterfly` entry points are paid once per BLOCK instead of once
    /// per 1-D transform (23 M of them on a 20 s 720p clip, 53 per block).
    ///
    /// # Safety
    /// `out` holds `N` writable `i32`; `src[k * s_in]` is readable for every
    /// `k < nzs[0]`; `tab` is the flat 32x32 DCT table.
    #[target_feature(enable = "avx2")]
    #[inline]
    unsafe fn idct_sums_avx2<const N: usize>(out: *mut i32, src: *const i32, s_in: usize, tab: *const i16, nzs: &[usize; 4]) {
        unsafe {
            match N {
                4 => accum_avx2::<4>(out, src, s_in, tab, 8, 0, 1, nzs[0]),
                8 => {
                    accum_avx2::<4>(out, src, s_in << 1, tab, 8, 0, 1, nzs[1]);
                    accum_butterfly_avx2::<4>(out, src, s_in, tab, 4, nzs[0]);
                }
                16 => {
                    accum_avx2::<4>(out, src, s_in << 2, tab, 8, 0, 1, nzs[2]);
                    accum_butterfly_avx2::<4>(out, src, s_in << 1, tab, 4, nzs[1]);
                    accum_butterfly_avx2::<8>(out, src, s_in, tab, 2, nzs[0]);
                }
                _ => {
                    accum_avx2::<4>(out, src, s_in << 3, tab, 8, 0, 1, nzs[3]);
                    accum_butterfly_avx2::<4>(out, src, s_in << 2, tab, 4, nzs[2]);
                    accum_butterfly_avx2::<8>(out, src, s_in << 1, tab, 2, nzs[1]);
                    accum_butterfly_avx2::<16>(out, src, s_in, tab, 1, nzs[0]);
                }
            }
        }
    }

    /// `nz` per butterfly level, exactly as `idct_sums` derives it: level `d`
    /// has size `n >> d`, and the live count halves with a `div_ceil` and
    /// clamps to the level's size.
    #[inline(always)]
    fn level_nzs(n: usize, nz: usize) -> [usize; 4] {
        let levels = n.trailing_zeros() as usize - 2;
        let mut nzs = [0usize; 4];
        let mut z = nz.min(n);
        let mut m = n;
        for slot in nzs.iter_mut().take(levels + 1) {
            *slot = z;
            if m > 4 {
                m /= 2;
                z = z.div_ceil(2).min(m);
            }
        }
        nzs
    }

    /// Both passes of an `N x N` inverse DCT (or the 4x4 DST when `DST`) in one
    /// kernel call.
    ///
    /// Stage 1 runs the `nz_w` live columns, clipping to the coefficient range
    /// into `tmp`; stage 2 runs every row over its `nz_w` live inputs into `d`.
    /// The arithmetic is the scalar path's, operation for operation -- only the
    /// call structure moved: `nzs` is derived once per block rather than once
    /// per 1-D transform (it is the same for every column, and again for every
    /// row), the sums never leave this frame, and the shift/clip of each 1-D
    /// result is a vector op on the sums rather than a dispatched kernel.
    ///
    /// # Safety
    /// `d` and `tmp` each hold `N * N` `i32`; `tab` is the flat 32x32 DCT
    /// table and `dst_tab` the flat 4x32 padded DST table; `nz_w, nz_h` in
    /// `1..=N`; `bd_shift` in `1..=31`.
    #[target_feature(enable = "avx2")]
    pub unsafe fn idct_block_avx2<const N: usize, const DST: bool>(d: *mut i32, tmp: *mut i32, nz_w: usize, nz_h: usize, bd_shift: u32, tab: *const i16, dst_tab: *const i16) {
        let mut sums = [0i32; 32];
        let sp = sums.as_mut_ptr();
        // Stage 1: columns, clipped to 16 bits.
        let nzs1 = level_nzs(N, nz_h);
        let add7 = _mm256_set1_epi32(64);
        let lo = _mm256_set1_epi32(super::COEFF_MIN);
        let hi = _mm256_set1_epi32(super::COEFF_MAX);
        for x in 0..nz_w {
            unsafe {
                if DST {
                    accum_avx2::<4>(sp, d.add(x), N, dst_tab, 1, 0, 1, nz_h.min(4));
                } else {
                    idct_sums_avx2::<N>(sp, d.add(x), N, tab, &nzs1);
                }
            }
            if N == 4 {
                let v = unsafe { _mm_loadu_si128(sp as *const __m128i) };
                let r = _mm_srai_epi32(_mm_add_epi32(v, _mm_set1_epi32(64)), 7);
                let r = _mm_min_epi32(_mm_max_epi32(r, _mm_set1_epi32(super::COEFF_MIN)), _mm_set1_epi32(super::COEFF_MAX));
                unsafe { _mm_storeu_si128(sp as *mut __m128i, r) };
            } else {
                let mut i = 0;
                while i < N {
                    let v = unsafe { _mm256_loadu_si256(sp.add(i) as *const __m256i) };
                    let r = _mm256_srai_epi32(_mm256_add_epi32(v, add7), 7);
                    let r = _mm256_min_epi32(_mm256_max_epi32(r, lo), hi);
                    unsafe { _mm256_storeu_si256(sp.add(i) as *mut __m256i, r) };
                    i += 8;
                }
            }
            // The column is strided in `tmp`; N scalar stores.
            for i in 0..N {
                unsafe { *tmp.add(i * N + x) = *sp.add(i) };
            }
        }
        // Stage 2: rows, over the `nz_w` live inputs, no clip.
        let nzs2 = level_nzs(N, nz_w);
        let addb = _mm256_set1_epi32(1i32 << (bd_shift - 1));
        let shb = _mm_cvtsi32_si128(bd_shift as i32);
        for y in 0..N {
            let row = unsafe { tmp.add(y * N) };
            let out = unsafe { d.add(y * N) };
            unsafe {
                if DST {
                    accum_avx2::<4>(sp, row, 1, dst_tab, 1, 0, 1, nz_w.min(4));
                } else {
                    idct_sums_avx2::<N>(sp, row, 1, tab, &nzs2);
                }
            }
            if N == 4 {
                let v = unsafe { _mm_loadu_si128(sp as *const __m128i) };
                let r = _mm_sra_epi32(_mm_add_epi32(v, _mm_set1_epi32(1i32 << (bd_shift - 1))), shb);
                unsafe { _mm_storeu_si128(out as *mut __m128i, r) };
            } else {
                let mut i = 0;
                while i < N {
                    let v = unsafe { _mm256_loadu_si256(sp.add(i) as *const __m256i) };
                    let r = _mm256_sra_epi32(_mm256_add_epi32(v, addb), shb);
                    unsafe { _mm256_storeu_si256(out.add(i) as *mut __m256i, r) };
                    i += 8;
                }
            }
        }
    }

    /// 4x4 inverse transform, both passes in registers.
    ///
    /// The butterfly kernels vectorise along the OUTPUT of one 1-D transform,
    /// which for a 4-point transform is half a register, eight times per block.
    /// Vectorising across the block's other dimension instead gives:
    ///
    /// ```text
    ///   stage 1   tmp[j] (a row over x) = clip( Σ_k T[k][j] · d[k] )     broadcast table, row vector
    ///   stage 2   out[y] (a row over j) =       Σ_k tmp[y][k] · T[k]     broadcast sample, table row
    /// ```
    ///
    /// -- no transpose, no scratch, no per-1-D call, every coefficient loaded
    /// once. Integer sums are exact in any order, so this is bit-identical to
    /// the scalar twin. `t` is the 4-point DCT or the DST.
    ///
    /// # Safety
    /// `d` holds 16 `i32`; `nz_w, nz_h` in `1..=4`; `bd_shift` in `1..=31`.
    #[target_feature(enable = "avx2")]
    pub unsafe fn idct4_avx2<const DST: bool>(d: *mut i32, bd_shift: u32) {
        // Every one of the sixteen coefficients is read: a 4x4 block is one
        // scan sub-block, so the parser cleared ALL of it (`scan_bbox` of a
        // 4x4 is the block), and a zero row contributes nothing. That is what
        // lets this be straight-line code with no `nz` bound and no loop --
        // the bounded-loop form measured 280 instructions a block, this ~100.
        let r0 = unsafe { _mm_loadu_si128(d as *const __m128i) };
        let r1 = unsafe { _mm_loadu_si128(d.add(4) as *const __m128i) };
        let r2 = unsafe { _mm_loadu_si128(d.add(8) as *const __m128i) };
        let r3 = unsafe { _mm_loadu_si128(d.add(12) as *const __m128i) };
        let c = |v: i32| _mm_set1_epi32(v);
        let mul = |a: __m128i, v: i32| _mm_mullo_epi32(a, c(v));
        // Stage 1: tmp[j] (a row over x) = Σ_k T[k][j] · r_k.
        let (t0, t1, t2, t3) = if DST {
            // DST4 rows [29,55,74,84] [74,74,0,-74] [84,-29,-74,55] [55,-84,74,-29].
            (
                _mm_add_epi32(_mm_add_epi32(mul(r0, 29), mul(r1, 74)), _mm_add_epi32(mul(r2, 84), mul(r3, 55))),
                _mm_add_epi32(_mm_add_epi32(mul(r0, 55), mul(r1, 74)), _mm_sub_epi32(mul(r3, -84), mul(r2, 29))),
                _mm_add_epi32(_mm_sub_epi32(mul(r0, 74), mul(r2, 74)), mul(r3, 74)),
                _mm_add_epi32(_mm_sub_epi32(mul(r0, 84), mul(r1, 74)), _mm_sub_epi32(mul(r2, 55), mul(r3, 29))),
            )
        } else {
            // Even part from rows 0 and 2 (64·(r0 ± r2), a shift), odd part
            // from rows 1 and 3 -- the 4-point butterfly.
            let e0 = _mm_slli_epi32(_mm_add_epi32(r0, r2), 6);
            let e1 = _mm_slli_epi32(_mm_sub_epi32(r0, r2), 6);
            let o0 = _mm_add_epi32(mul(r1, 83), mul(r3, 36));
            let o1 = _mm_sub_epi32(mul(r1, 36), mul(r3, 83));
            (_mm_add_epi32(e0, o0), _mm_add_epi32(e1, o1), _mm_sub_epi32(e1, o1), _mm_sub_epi32(e0, o0))
        };
        let add7 = c(64);
        let lo = c(super::COEFF_MIN);
        let hi = c(super::COEFF_MAX);
        let clip = |v: __m128i| _mm_min_epi32(_mm_max_epi32(_mm_srai_epi32(_mm_add_epi32(v, add7), 7), lo), hi);
        let tmp = [clip(t0), clip(t1), clip(t2), clip(t3)];
        // Stage 2: out[y] (a row over j) = Σ_k tmp[y][k] · T[k].
        let (k0, k1, k2, k3) = if DST {
            (
                _mm_setr_epi32(29, 55, 74, 84),
                _mm_setr_epi32(74, 74, 0, -74),
                _mm_setr_epi32(84, -29, -74, 55),
                _mm_setr_epi32(55, -84, 74, -29),
            )
        } else {
            (
                _mm_setr_epi32(64, 64, 64, 64),
                _mm_setr_epi32(83, 36, -36, -83),
                _mm_setr_epi32(64, -64, -64, 64),
                _mm_setr_epi32(36, -83, 83, -36),
            )
        };
        let addb = c(1i32 << (bd_shift - 1));
        let shb = _mm_cvtsi32_si128(bd_shift as i32);
        for (y, &ty) in tmp.iter().enumerate() {
            let acc = _mm_add_epi32(
                _mm_add_epi32(_mm_mullo_epi32(_mm_shuffle_epi32(ty, 0x00), k0), _mm_mullo_epi32(_mm_shuffle_epi32(ty, 0x55), k1)),
                _mm_add_epi32(_mm_mullo_epi32(_mm_shuffle_epi32(ty, 0xAA), k2), _mm_mullo_epi32(_mm_shuffle_epi32(ty, 0xFF), k3)),
            );
            unsafe { _mm_storeu_si128(d.add(y * 4) as *mut __m128i, _mm_sra_epi32(_mm_add_epi32(acc, addb), shb)) };
        }
    }

    /// 8x8 inverse DCT, both passes in registers. Same scheme as
    /// [`idct4_avx2`] at a full `ymm` per row.
    ///
    /// # Safety
    /// `d` holds 64 `i32`; `nz_w, nz_h` in `1..=8`; `bd_shift` in `1..=31`.
    #[target_feature(enable = "avx2")]
    pub unsafe fn idct8_avx2(d: *mut i32, nz_w: usize, nz_h: usize, bd_shift: u32, t: &[[i32; 8]; 8]) {
        let zero = _mm256_setzero_si256();
        let mut rows = [zero; 8];
        for (k, r) in rows.iter_mut().enumerate().take(nz_h) {
            *r = unsafe { _mm256_loadu_si256(d.add(k * 8) as *const __m256i) };
        }
        let add7 = _mm256_set1_epi32(64);
        let lo = _mm256_set1_epi32(super::COEFF_MIN);
        let hi = _mm256_set1_epi32(super::COEFF_MAX);
        let mut tmp = [zero; 8];
        for (j, o) in tmp.iter_mut().enumerate() {
            let mut acc = zero;
            for k in 0..nz_h {
                acc = _mm256_add_epi32(acc, _mm256_mullo_epi32(_mm256_set1_epi32(t[k][j]), rows[k]));
            }
            *o = _mm256_min_epi32(_mm256_max_epi32(_mm256_srai_epi32(_mm256_add_epi32(acc, add7), 7), lo), hi);
        }
        let addb = _mm256_set1_epi32(1i32 << (bd_shift - 1));
        let shb = _mm_cvtsi32_si128(bd_shift as i32);
        let mut trow = [zero; 8];
        for (k, r) in trow.iter_mut().enumerate().take(nz_w) {
            *r = unsafe { _mm256_loadu_si256(t[k].as_ptr() as *const __m256i) };
        }
        for (y, &ty) in tmp.iter().enumerate() {
            let mut acc = zero;
            for k in 0..nz_w {
                // Lane `k` of `tmp[y]` broadcast to all eight lanes.
                let b = _mm256_permutevar8x32_epi32(ty, _mm256_set1_epi32(k as i32));
                acc = _mm256_add_epi32(acc, _mm256_mullo_epi32(b, trow[k]));
            }
            unsafe { _mm256_storeu_si256(d.add(y * 8) as *mut __m256i, _mm256_sra_epi32(_mm256_add_epi32(acc, addb), shb)) };
        }
    }

    // ---- SSE4.1 mirrors -----------------------------------------------------
    //
    // Four `i32` lanes instead of eight, and otherwise the same kernel: SSE4.1
    // is where `pmulld`, `pmovsxwd` and `pminsd`/`pmaxsd` arrive, which is
    // exactly the instruction set this transform needs and exactly what SSE2
    // lacks. Before this rung existed, every pre-2013 machine ran the whole
    // transform scalar.

    /// # Safety
    /// As [`accum_avx2`], with `LEN` a multiple of 4 up to 16.
    #[target_feature(enable = "sse4.1")]
    pub unsafe fn accum_sse41<const LEN: usize>(out: *mut i32, src: *const i32, s_in: usize, tab: *const i16, tstep: usize, k0: usize, kstep: usize, nz: usize) {
        let mut acc = [_mm_setzero_si128(); 4];
        let mut k = k0;
        while k < nz {
            let c = unsafe { *src.add(k * s_in) };
            if c != 0 {
                let row = unsafe { tab.add(k * tstep * 32) };
                let cv = _mm_set1_epi32(c);
                let mut b = 0;
                while b < LEN / 4 {
                    let t = _mm_cvtepi16_epi32(unsafe { _mm_loadl_epi64(row.add(b * 4) as *const __m128i) });
                    acc[b] = _mm_add_epi32(acc[b], _mm_mullo_epi32(t, cv));
                    b += 1;
                }
            }
            k += kstep;
        }
        let mut b = 0;
        while b < LEN / 4 {
            unsafe { _mm_storeu_si128(out.add(b * 4) as *mut __m128i, acc[b]) };
            b += 1;
        }
    }

    /// # Safety
    /// As [`accum_butterfly_avx2`], with `HALF` a multiple of 4 up to 16.
    #[target_feature(enable = "sse4.1")]
    pub unsafe fn accum_butterfly_sse41<const HALF: usize>(out: *mut i32, src: *const i32, s_in: usize, tab: *const i16, tstep: usize, nz: usize) {
        let mut acc = [_mm_setzero_si128(); 4];
        let mut k = 1;
        while k < nz {
            let c = unsafe { *src.add(k * s_in) };
            if c != 0 {
                let row = unsafe { tab.add(k * tstep * 32) };
                let cv = _mm_set1_epi32(c);
                let mut b = 0;
                while b < HALF / 4 {
                    let t = _mm_cvtepi16_epi32(unsafe { _mm_loadl_epi64(row.add(b * 4) as *const __m128i) });
                    acc[b] = _mm_add_epi32(acc[b], _mm_mullo_epi32(t, cv));
                    b += 1;
                }
            }
            k += 2;
        }
        let n = 2 * HALF;
        let mut b = 0;
        while b < HALF / 4 {
            let j = b * 4;
            let e = unsafe { _mm_loadu_si128(out.add(j) as *const __m128i) };
            unsafe { _mm_storeu_si128(out.add(j) as *mut __m128i, _mm_add_epi32(e, acc[b])) };
            // 0x1b reverses the four lanes, so `out[n-1-j] ..= out[n-4-j]`
            // descending becomes one contiguous store at `n - 4 - j`.
            let d = _mm_shuffle_epi32(_mm_sub_epi32(e, acc[b]), 0x1b);
            unsafe { _mm_storeu_si128(out.add(n - 4 - j) as *mut __m128i, d) };
            b += 1;
        }
    }

    /// # Safety
    /// Both pointers hold `n` elements and `n` is a positive multiple of 4.
    #[target_feature(enable = "sse4.1")]
    pub unsafe fn shift_clip_sse41<const CLIP: bool>(dst: *mut i32, src: *const i32, n: usize, shift: u32, lo: i32, hi: i32) {
        let addv = _mm_set1_epi32(1i32 << (shift - 1));
        let sh = _mm_cvtsi32_si128(shift as i32);
        let lov = _mm_set1_epi32(lo);
        let hiv = _mm_set1_epi32(hi);
        // Two vectors per trip, as in the AVX2 twin.
        let one = |i: usize| {
            let v = unsafe { _mm_loadu_si128(src.add(i) as *const __m128i) };
            let mut r = _mm_sra_epi32(_mm_add_epi32(v, addv), sh);
            if CLIP {
                r = _mm_min_epi32(_mm_max_epi32(r, lov), hiv);
            }
            unsafe { _mm_storeu_si128(dst.add(i) as *mut __m128i, r) };
        };
        let mut i = 0;
        while i + 8 <= n {
            one(i);
            one(i + 4);
            i += 8;
        }
        while i + 4 <= n {
            one(i);
            i += 4;
        }
    }
}

/// The instruction set these kernels will actually use, cached.
///
/// One relaxed load for the whole decision, rather than an env lookup and an
/// `isa()` probe on every one of millions of calls. `RH265_SCALAR_ITX` reports
/// `Scalar`, which is how the bring-up switch and the ISA rung share a path.
#[inline(always)]
fn kernel_isa() -> crate::Isa {
    use std::sync::OnceLock;
    static LVL: OnceLock<crate::Isa> = OnceLock::new();
    *LVL.get_or_init(|| if std::env::var_os("RH265_SCALAR_ITX").is_some() { crate::Isa::Scalar } else { crate::isa() })
}

/// Whether any vector arm is available.
#[inline(always)]
fn use_kernel() -> bool {
    kernel_isa() >= crate::Isa::Sse41
}

/// `out[j] = sum_k src[k * s_in] * tab[k * tstep][j]` for `j < len`, over
/// `k = k0, k0 + kstep, ... < nz`.
///
/// `len` is always 4, 8 or 16 (a butterfly half, or the 4-point base case), so
/// the dispatch is a match on three const-generic shapes rather than a loop
/// bound the kernel has to carry.
#[allow(clippy::too_many_arguments)]
pub fn accum(out: &mut [i32], src: &[i32], s_in: usize, tab: &[i16], tstep: usize, k0: usize, kstep: usize, nz: usize, len: usize) {
    #[cfg(all(feature = "simd", target_arch = "x86_64"))]
    {
        // Bounds, proven once here so the kernel needs no checks.
        //
        // `kmax` was `k0 + ((nz - 1 - k0) / kstep) * kstep` -- an integer
        // DIVISION, in a function called 2.47 million times a clip. `nz - 1`
        // bounds every k this can touch for either call site (`kstep` is 1 or
        // 2), which is the same guarantee without the divide.
        let kmax = nz.saturating_sub(1);
        let ok = out.len() >= len && (nz <= k0 || (src.len() > kmax * s_in && tab.len() >= kmax * tstep * 32 + len)) && matches!(len, 4 | 8 | 16);
        // ONE relaxed load for the whole decision. It was two -- a `OnceLock`
        // for the switch and `isa()` for the ISA -- and `isa()`'s own docs say
        // callers hoist it above their loops. This is that hoist: the answer
        // cannot change during a run, so it is computed once per process.
        if ok && use_kernel() {
            if census::ALWAYS {
                census::bump(&census::ITX_ACCUM_SIMD, 1);
            }
            // SAFETY: `ok` covers every load and store the kernel makes.
            unsafe {
                let (o, s, t) = (out.as_mut_ptr(), src.as_ptr(), tab.as_ptr());
                if kernel_isa() >= crate::Isa::Avx2 {
                    match len {
                        4 => x86::accum_avx2::<4>(o, s, s_in, t, tstep, k0, kstep, nz),
                        8 => x86::accum_avx2::<8>(o, s, s_in, t, tstep, k0, kstep, nz),
                        _ => x86::accum_avx2::<16>(o, s, s_in, t, tstep, k0, kstep, nz),
                    }
                } else {
                    match len {
                        4 => x86::accum_sse41::<4>(o, s, s_in, t, tstep, k0, kstep, nz),
                        8 => x86::accum_sse41::<8>(o, s, s_in, t, tstep, k0, kstep, nz),
                        _ => x86::accum_sse41::<16>(o, s, s_in, t, tstep, k0, kstep, nz),
                    }
                }
            }
            return;
        }
    }
    if census::ALWAYS {
        census::bump(&census::ITX_ACCUM_SCALAR, 1);
    }
    accum_scalar(out, src, s_in, tab, tstep, k0, kstep, nz, len);
}

/// The butterfly's output stage, scalar.
///
/// Kept because [`accum_butterfly_scalar`] is built from it; the SIMD arm folds
/// this into the accumulate rather than calling it, which is why there is no
/// standalone vector twin -- a kernel with no production caller is a defect, not
/// a spare part.
pub fn butterfly_scalar(out: &mut [i32], odd: &[i32], n: usize) {
    let half = n / 2;
    for j in 0..half {
        let even = out[j];
        out[j] = even + odd[j];
        out[n - 1 - j] = even - odd[j];
    }
}

/// `#[cold]`: the SIMD guard above it succeeds on every block of a conformant
/// stream -- the `*_SCALAR` census counters read 0 across the corpus -- so this
/// is the fallback for a shape the kernels decline, not a path decode takes.
/// Inlined it padded the dispatcher, which IS on the hot path, with a body that
/// never runs. Same reason `Cabac::refill_tail` is out of line.
#[cold]
#[inline(never)]
/// Scalar twin of [`shift_clip`].
pub fn shift_clip_scalar<const CLIP: bool>(dst: &mut [i32], src: &[i32], n: usize, shift: u32, lo: i32, hi: i32) {
    let add = 1i32 << (shift - 1);
    for i in 0..n {
        let v = (src[i] + add) >> shift;
        dst[i] = if CLIP { v.clamp(lo, hi) } else { v };
    }
}

/// `dst[i] = ((src[i] + 2^(shift-1)) >> shift)`, clamped when `CLIP`.
///
/// The final pass of every 1-D transform: `n` per transform, and the second
/// stage's destination is contiguous, which is the majority of the population
/// (`n` row transforms against `nz_w` column transforms).
pub fn shift_clip<const CLIP: bool>(dst: &mut [i32], src: &[i32], n: usize, shift: u32, lo: i32, hi: i32) {
    #[cfg(all(feature = "simd", target_arch = "x86_64"))]
    {
        if dst.len() >= n && src.len() >= n && use_kernel() && n % 4 == 0 && n >= 4 {
            if census::ALWAYS {
                census::bump(&census::ITX_SHIFT_SIMD, 1);
            }
            // SAFETY: both slices hold `n` elements, `n` a multiple of 4
            // (and of 8 for the wide arm).
            unsafe {
                if kernel_isa() >= crate::Isa::Avx2 && n % 8 == 0 {
                    x86::shift_clip_avx2::<CLIP>(dst.as_mut_ptr(), src.as_ptr(), n, shift, lo, hi);
                } else {
                    x86::shift_clip_sse41::<CLIP>(dst.as_mut_ptr(), src.as_ptr(), n, shift, lo, hi);
                }
            }
            return;
        }
    }
    if census::ALWAYS {
        census::bump(&census::ITX_SHIFT_SCALAR, 1);
    }
    shift_clip_scalar::<CLIP>(dst, src, n, shift, lo, hi);
}

/// Scalar twin of [`accum_butterfly`].
pub fn accum_butterfly_scalar(out: &mut [i32], src: &[i32], s_in: usize, tab: &[i16], tstep: usize, nz: usize, n: usize) {
    let half = n / 2;
    let mut odd = [0i32; 16];
    accum_scalar(&mut odd[..half], src, s_in, tab, tstep, 1, 2, nz, half);
    butterfly_scalar(out, &odd[..half], n);
}

/// The odd accumulation and the butterfly output stage as one pass, so the odd
/// sums never round-trip through memory.
#[allow(clippy::too_many_arguments)]
pub fn accum_butterfly(out: &mut [i32], src: &[i32], s_in: usize, tab: &[i16], tstep: usize, nz: usize, n: usize) {
    #[cfg(all(feature = "simd", target_arch = "x86_64"))]
    {
        let half = n / 2;
        let kmax = nz.saturating_sub(1);
        let ok = out.len() >= n && matches!(half, 4 | 8 | 16) && (nz <= 1 || (src.len() > kmax * s_in && tab.len() >= kmax * tstep * 32 + half));
        if ok && use_kernel() {
            if census::ALWAYS {
                census::bump(&census::ITX_FUSED_SIMD, 1);
            }
            // SAFETY: `ok` covers every access the kernel makes.
            unsafe {
                let (o, sp, t) = (out.as_mut_ptr(), src.as_ptr(), tab.as_ptr());
                if kernel_isa() >= crate::Isa::Avx2 {
                    match half {
                        4 => x86::accum_butterfly_avx2::<4>(o, sp, s_in, t, tstep, nz),
                        8 => x86::accum_butterfly_avx2::<8>(o, sp, s_in, t, tstep, nz),
                        _ => x86::accum_butterfly_avx2::<16>(o, sp, s_in, t, tstep, nz),
                    }
                } else {
                    match half {
                        4 => x86::accum_butterfly_sse41::<4>(o, sp, s_in, t, tstep, nz),
                        8 => x86::accum_butterfly_sse41::<8>(o, sp, s_in, t, tstep, nz),
                        _ => x86::accum_butterfly_sse41::<16>(o, sp, s_in, t, tstep, nz),
                    }
                }
            }
            return;
        }
    }
    if census::ALWAYS {
        census::bump(&census::ITX_FUSED_SCALAR, 1);
    }
    accum_butterfly_scalar(out, src, s_in, tab, tstep, nz, n);
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The kernel against the scalar oracle, over every shape the butterfly
    /// produces: both accumulation sites (`k0`/`kstep` of 0/1 and 1/2), all
    /// three lengths, every `nz`, and the strides the two transform passes use.
    ///
    /// Coefficients are drawn across the full clipped range including zero, so
    /// the per-coefficient skip is exercised rather than assumed.
    #[test]
    fn accum_matches_scalar() {
        let mut st = 0x17c0_ffeeu32;
        let rnd = |s: &mut u32| {
            *s = s.wrapping_mul(1_664_525).wrapping_add(1_013_904_223);
            ((*s >> 8) as i32 % 65536) - 32768
        };
        // A stand-in for DCT32: 32 rows of 32 `i16` in the real table's range.
        let tab: Vec<i16> = (0..32 * 32).map(|_| (rnd(&mut st) % 91) as i16).collect();
        let mut checked = 0usize;
        for &len in &[4usize, 8, 16] {
            for &tstep in &[1usize, 2, 4, 8] {
                for &(k0, kstep) in &[(0usize, 1usize), (1, 2)] {
                    for nz in 1..=32usize {
                        if nz > 32 / tstep.max(1) {
                            continue;
                        }
                        for &s_in in &[1usize, 32, 35] {
                            let src: Vec<i32> = (0..32 * s_in + 8)
                                .map(|_| {
                                    let v = rnd(&mut st);
                                    if v % 3 == 0 {
                                        0
                                    } else {
                                        v
                                    }
                                })
                                .collect();
                            let mut a = vec![0i32; len];
                            let mut b = vec![0i32; len];
                            accum_scalar(&mut a, &src, s_in, &tab, tstep, k0, kstep, nz, len);
                            accum(&mut b, &src, s_in, &tab, tstep, k0, kstep, nz, len);
                            assert_eq!(a, b, "len={len} tstep={tstep} k0={k0} kstep={kstep} nz={nz} s_in={s_in}");
                            checked += 1;
                        }
                    }
                }
            }
        }
        assert!(checked > 500, "sweep collapsed to {checked} cases");
    }

    /// The shift-and-clip stage against its scalar twin, both arms of `CLIP`,
    /// including values that land outside the coefficient range so the clamp is
    /// exercised rather than assumed.
    #[test]
    fn shift_clip_matches_scalar() {
        let mut st = 0x5417_c11au32;
        let rnd = |s: &mut u32| {
            *s = s.wrapping_mul(1_664_525).wrapping_add(1_013_904_223);
            (*s >> 4) as i32 - (1 << 27)
        };
        for &n in &[8usize, 16, 32] {
            for &shift in &[7u32, 10, 12] {
                let src: Vec<i32> = (0..n).map(|_| rnd(&mut st)).collect();
                let mut a = vec![0i32; n];
                let mut b = vec![0i32; n];
                shift_clip_scalar::<true>(&mut a, &src, n, shift, -32768, 32767);
                shift_clip::<true>(&mut b, &src, n, shift, -32768, 32767);
                assert_eq!(a, b, "clip n={n} shift={shift}");
                shift_clip_scalar::<false>(&mut a, &src, n, shift, -32768, 32767);
                shift_clip::<false>(&mut b, &src, n, shift, -32768, 32767);
                assert_eq!(a, b, "noclip n={n} shift={shift}");
            }
        }
    }

    /// The fused accumulate-and-butterfly against the two-step scalar form.
    ///
    /// Fusing moves the odd sums from a scratch array into registers, so a
    /// mistake shows as the high half being right and the low half stale, or
    /// vice versa -- this compares the whole output, both halves, at every size.
    #[test]
    fn accum_butterfly_matches_scalar() {
        let mut st = 0xfabc_0de1u32;
        let rnd = |s: &mut u32| {
            *s = s.wrapping_mul(1_664_525).wrapping_add(1_013_904_223);
            ((*s >> 8) as i32 % 65536) - 32768
        };
        let tab: Vec<i16> = (0..32 * 32).map(|_| (rnd(&mut st) % 91) as i16).collect();
        let mut checked = 0usize;
        for &n in &[8usize, 16, 32] {
            // `tstep` is not free: the butterfly reaches row `k * (32 / n)` of a
            // 32-row table, so the two are linked. Sweeping them independently
            // generates combinations the decoder cannot produce -- and the first
            // version of this test did, walking the SCALAR twin off the end of
            // the table. The oracle found it, which is the point of having one.
            {
                let tstep = 32 / n;
                for nz in 1..=n {
                    for &s_in in &[1usize, 32, 35] {
                        for _rep in 0..3 {
                            let src: Vec<i32> = (0..32 * s_in + 8)
                                .map(|_| {
                                    let v = rnd(&mut st);
                                    if v % 3 == 0 {
                                        0
                                    } else {
                                        v
                                    }
                                })
                                .collect();
                            let base: Vec<i32> = (0..n).map(|_| rnd(&mut st)).collect();
                            let mut a = base.clone();
                            let mut b = base.clone();
                            accum_butterfly_scalar(&mut a, &src, s_in, &tab, tstep, nz, n);
                            accum_butterfly(&mut b, &src, s_in, &tab, tstep, nz, n);
                            assert_eq!(a, b, "n={n} tstep={tstep} nz={nz} s_in={s_in}");
                            checked += 1;
                        }
                    }
                }
            }
        }
        assert!(checked > 300, "sweep collapsed to {checked}");
    }
}
