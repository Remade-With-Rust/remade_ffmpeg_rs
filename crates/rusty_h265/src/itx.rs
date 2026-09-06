//! Scaling (§8.6.3) and inverse transforms (§8.6.4): the 4/8/16/32-point
//! inverse DCT, the 4x4 inverse DST, transform skip and transquant bypass.
//! Integer only.
//!
//! # Sparsity is the structure that matters here
//!
//! A coded transform block is nearly always sparse: the entropy coder signals
//! the position of the last significant coefficient, and everything past it in
//! scan order is zero. A 32x32 block whose non-zero coefficients live in an
//! 8x8 corner needs an eighth of the arithmetic a dense implementation does,
//! and no amount of vectorisation recovers work that never needed doing.
//!
//! So every entry point takes the non-zero rectangle `(nz_w, nz_h)` that the
//! residual parser observed, and:
//!
//! - **scaling** touches only that rectangle;
//! - the **column pass** runs only over the `nz_w` columns that can be
//!   non-zero, and reads only the `nz_h` rows within them — the rest of the
//!   intermediate is zeroed, not transformed;
//! - the **row pass** reads only the `nz_w` inputs each row can have.
//!
//! For the dense case (`nz_w = nz_h = n`) this is exactly the old arithmetic,
//! so the conformance suite gates both regimes at once.

use crate::tables::{DCT32, DST4, LEVEL_SCALE};
use rusty_h265_accel as accel;

pub const COEFF_MIN: i32 = -32768;
pub const COEFF_MAX: i32 = 32767;

/// Which residual path a transform block takes.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TransformKind {
    Dct,
    Dst,
    Skip,
    Bypass,
}

/// §8.6.3: `TransCoeffLevel` -> scaled coefficients `d`, in place (raster
/// order, `n x n`), over the non-zero rectangle only. `m` is the scaling
/// factor per position (`None` = flat 16).
pub fn dequant(coeffs: &mut [i32], n: usize, nz_w: usize, nz_h: usize, qp: i32, bit_depth: u8, m: Option<&[u8]>) {
    let log2n = n.trailing_zeros() as i32;
    let bd_shift = bit_depth as i32 + log2n - 5;
    let scale = LEVEL_SCALE[(qp % 6) as usize] << (qp / 6);
    let add = 1i64 << (bd_shift - 1);
    for y in 0..nz_h {
        for x in 0..nz_w {
            let i = y * n + x;
            let c = coeffs[i];
            if c == 0 {
                continue;
            }
            let f = m.map_or(16, |t| t[i] as i32);
            let v = ((c as i64 * f as i64 * scale as i64) + add) >> bd_shift;
            coeffs[i] = v.clamp(COEFF_MIN as i64, COEFF_MAX as i64) as i32;
        }
    }
}

/// Bring-up switch: `RH265_NO_DC_FAST=1` sends DC-only blocks down the general
/// transform, so the collapse can be A/B'd inside one binary. Default OFF —
/// the fast path ships.
/// `RH265_NAIVE_IDCT=1` runs the naive `N²` sums instead of the butterfly.
/// Diagnostic only; read once per block by `inverse_transform`.
fn naive_idct() -> bool {
    use std::sync::OnceLock;
    static ON: OnceLock<bool> = OnceLock::new();
    *ON.get_or_init(|| std::env::var_os("RH265_NAIVE_IDCT").is_some())
}

fn no_dc_fast() -> bool {
    static F: std::sync::OnceLock<bool> = std::sync::OnceLock::new();
    *F.get_or_init(|| std::env::var_os("RH265_NO_DC_FAST").is_some())
}

/// Partial-sum core of the inverse DCT, by **partial butterfly**.
///
/// The naive form evaluates `out[j] = Σ_k src[k]·T[k][j]` independently for
/// every output — `N²` multiplies, 1024 of them for a 32-point transform. The
/// matrix does not require that. Its even rows are symmetric and its odd rows
/// antisymmetric about the midpoint (asserted by
/// `transform_matrix_has_the_butterfly_symmetry`), so splitting the sum by the
/// parity of `k`:
///
/// ```text
///   E[j] = Σ_m src[2m]·T[2m][j]        (even part, symmetric in j)
///   O[j] = Σ_m src[2m+1]·T[2m+1][j]    (odd part, antisymmetric in j)
///
///   out[j]     = E[j] + O[j]
///   out[N−1−j] = E[j] − O[j]
/// ```
///
/// Two outputs now come from one pair of partial sums. And `E` is not merely
/// half the work — the even rows of the N-point matrix, sampled at the first
/// `N/2` columns, ARE the (N/2)-point matrix, so the even half is a smaller
/// transform of the even-indexed coefficients and the recursion continues:
///
/// ```text
///   T(N) = T(N/2) + (N/2)²      →   T(32) = 352 multiplies, not 1024
/// ```
///
/// This is exact integer arithmetic reassociated, not an approximation:
/// addition is associative, the accumulator is `i64`, and the result is
/// bit-identical to the naive order. `idct_sums_naive` stays in the tree as the
/// oracle and `butterfly_matches_naive` pins them together.
///
/// `E` is written straight into `out[..half]` and expanded in place — for
/// `j < half`, `N−1−j ≥ half`, so the write never clobbers a value still to be
/// read. That keeps the routine scratch-free, which matters: a per-call
/// temporary here would be paid once per row and column of every transform
/// block.
fn idct_sums(src: &[i32], s_in: usize, n: usize, nz: usize, out: &mut [i32]) {
    // `32 / n` with n a power of two.
    let step = 32 >> n.trailing_zeros();
    let nz = nz.min(n);
    if n == 4 {
        out[..4].fill(0);
        for k in 0..nz {
            let c = src[k * s_in];
            if c == 0 {
                continue;
            }
            let row = &DCT32[k * step];
            for j in 0..4 {
                out[j] += c * row[j] as i32;
            }
        }
        return;
    }
    let half = n / 2;
    // Even coefficients, at twice the stride, are the (N/2)-point transform.
    idct_sums(src, 2 * s_in, half, nz.div_ceil(2), &mut out[..half]);

    // The odd part, accumulated COEFFICIENT-OUTER rather than output-outer.
    //
    // Written the other way round -- `for j { for k { odd += c * T[k][j] } }` --
    // every one of the N²/4 inner steps reloads `src[k * s_in]`, recomputes the
    // row address `k * step`, and re-tests `c != 0`, and it walks the table
    // DOWN a column, which is a stride of 32 `i16`. None of that is the
    // arithmetic; all of it is per-(j,k) instead of per-k.
    //
    // Interchanged, each coefficient is loaded once, its row address is formed
    // once, the zero test skips a whole row instead of one product, and the
    // inner loop is a contiguous scan of `T[k][..]` -- which is the shape a
    // vector unit wants.
    let mut odd = [0i32; 16];
    let mut k = 1;
    while k < nz {
        let c = src[k * s_in];
        if c != 0 {
            let row = &DCT32[k * step];
            for j in 0..half {
                odd[j] += c * row[j] as i32;
            }
        }
        k += 2;
    }
    for j in 0..half {
        let even = out[j];
        out[j] = even + odd[j];
        out[n - 1 - j] = even - odd[j];
    }
}

/// The naive `N²` form. Kept as the oracle for [`idct_sums`], and reachable at
/// runtime via `RH265_NAIVE_IDCT=1` so the butterfly can be A/B'd inside one
/// binary.
fn idct_sums_naive(src: &[i32], s_in: usize, n: usize, nz: usize, out: &mut [i32]) {
    let step = 32 >> n.trailing_zeros();
    for (j, o) in out[..n].iter_mut().enumerate() {
        let mut sum = 0i32;
        for k in 0..nz.min(n) {
            let c = src[k * s_in];
            if c != 0 {
                sum += c * DCT32[k * step][j] as i32;
            }
        }
        *o = sum;
    }
}

/// One-dimensional N-point inverse transform of `src` (stride `s_in`) into
/// `dst` (stride `s_out`), reading `nz` inputs, `shift` with rounding, clipped
/// to 16 bits when `clip` (the first stage).
#[inline]
fn idct_1d<const CLIP: bool>(src: &[i32], s_in: usize, dst: &mut [i32], s_out: usize, n: usize, nz: usize, shift: u32, naive: bool) {
    // `i32`, not `i64`. The accumulator's worst case is 61,014,016 against an
    // `i32` ceiling of 2,147,483,647 -- 35x of headroom, asserted from the table
    // itself by `transform_accumulator_fits_i32`. The `i64` it replaced cost a
    // widening conversion per coefficient, double the register pressure, and
    // half the lanes of any vector form of the loop above.
    let mut sums = [0i32; 32];
    if naive {
        idct_sums_naive(src, s_in, n, nz, &mut sums[..n]);
    } else {
        idct_sums(src, s_in, n, nz, &mut sums[..n]);
    }
    let add = 1i32 << (shift - 1);
    for i in 0..n {
        let v = (sums[i] + add) >> shift;
        // `CLIP` is a const generic: the first pass clips to the coefficient
        // range and the second does not, and which one it is was a runtime bool
        // tested once per OUTPUT SAMPLE.
        dst[i * s_out] = if CLIP { v.clamp(COEFF_MIN, COEFF_MAX) } else { v };
    }
}

#[inline]
fn idst_1d<const CLIP: bool>(src: &[i32], s_in: usize, dst: &mut [i32], s_out: usize, nz: usize, shift: u32) {
    // The 4-point DST, coefficient-outer, in `i32`.
    //
    //   DST4 = [[29, 55, 74, 84], [74, 74, 0, -74], [84, -29, -74, 55], [55, -84, 74, -29]]
    //
    // Same interchange as the DCT: one load and one zero-test per coefficient
    // instead of per product, and a contiguous row scan. Row 1 carries a zero,
    // so a third of its products were already nothing.
    let add = 1i32 << (shift - 1);
    let mut sum = [0i32; 4];
    for k in 0..nz.min(4) {
        let c = src[k * s_in];
        if c == 0 {
            continue;
        }
        let row = &DST4[k];
        for i in 0..4 {
            sum[i] += c * row[i] as i32;
        }
    }
    for i in 0..4 {
        let v = (sum[i] + add) >> shift;
        dst[i * s_out] = if CLIP { v.clamp(COEFF_MIN, COEFF_MAX) } else { v };
    }
}

/// §8.6.4: scaled coefficients `d` (raster, `n x n`) -> residual `r`, in place.
///
/// `nz_w` / `nz_h` bound the non-zero coefficients; pass `n` for both if the
/// caller does not know.
/// `tmp` is the stage-1 intermediate, supplied by the caller and reused across
/// blocks.
///
/// It used to be a `[0i32; 32 * 32]` local — **4 KB of zeroing per transform
/// block**, which on a 4x4 block is 256 bytes of memset per coefficient. None
/// of it was needed: stage 1 writes every element stage 2 reads (columns
/// `0..nz_w`, all `n` rows), so the initial value is never observed. Taking it
/// from the caller makes that explicit and pays the zeroing once per picture.
pub fn inverse_transform(d: &mut [i32], tmp: &mut [i32], n: usize, nz_w: usize, nz_h: usize, bit_depth: u8, kind: TransformKind) {
    let bd_shift = 20 - bit_depth as u32;
    match kind {
        TransformKind::Bypass => {}
        TransformKind::Skip => rusty_h265_accel::pixel::transform_skip(d, n, bd_shift),
        // A DC-only DCT block is one number, not a transform.
        //
        // Row 0 of the DCT matrix is the constant 64, so with a single non-zero
        // coefficient both passes degenerate:
        //
        //   stage 1:  tmp[i] = clip((d[0]·64 + 64) >> 7)          — same for every i
        //   stage 2:  out[j] = (tmp·64 + 2^(s−1)) >> s            — same for every j
        //
        // so the whole N×N output is one value and the `N²` multiplies of the
        // second pass are two. This is the same symmetry the butterfly uses,
        // taken to its limit: the DST is excluded because `DST4[0]` is
        // `[29, 55, 74, 84]`, not a constant.
        TransformKind::Dct if nz_w.max(1) == 1 && nz_h.max(1) == 1 && !no_dc_fast() => {
            accel::census::arm(&accel::census::RT_TX_DC_ONLY);
            let v1 = (((d[0] as i64 * 64 + 64) >> 7) as i32).clamp(COEFF_MIN, COEFF_MAX);
            let add = 1i64 << (bd_shift - 1);
            let out = ((v1 as i64 * 64 + add) >> bd_shift) as i32;
            d[..n * n].fill(out);
        }
        TransformKind::Dct | TransformKind::Dst => {
            accel::census::arm(&accel::census::RT_TX_GENERAL);
            let nz_w = nz_w.clamp(1, n);
            let nz_h = nz_h.clamp(1, n);
            // Read ONCE per block, not once per 1-D transform. It used to sit
            // inside `idct_1d`, which runs `nz_w + n` times per block and
            // millions of times per picture, so a diagnostic switch was paying
            // an atomic load on the hot path.
            let naive = naive_idct();
            // Stage 1: columns. Only the first `nz_w` can hold anything; the
            // rest of the intermediate stays zero, which the row pass then
            // skips through `nz_w`.
            for x in 0..nz_w {
                if kind == TransformKind::Dst {
                    idst_1d::<true>(&d[x..], n, &mut tmp[x..], n, nz_h, 7);
                } else {
                    idct_1d::<true>(&d[x..], n, &mut tmp[x..], n, n, nz_h, 7, naive);
                }
            }
            // Stage 2: rows, over the `nz_w` inputs each row can have.
            for y in 0..n {
                if kind == TransformKind::Dst {
                    idst_1d::<false>(&tmp[y * n..], 1, &mut d[y * n..], 1, nz_w, bd_shift);
                } else {
                    idct_1d::<false>(&tmp[y * n..], 1, &mut d[y * n..], 1, n, nz_w, bd_shift, naive);
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn dc_only_block_is_flat() {
        let mut tmp = vec![0i32; 32 * 32];
        for &n in &[4usize, 8, 16, 32] {
            let mut d = vec![0i32; n * n];
            d[0] = 64 * 8;
            inverse_transform(&mut d, &mut tmp, n, 1, 1, 8, TransformKind::Dct);
            let v = d[0];
            assert!(d.iter().all(|&x| x == v), "n={n}");
            assert_eq!(v, 4, "n={n}");
        }
    }

    /// The whole point of the sparse bounds: transforming with the true
    /// non-zero rectangle must equal transforming the block as dense.
    #[test]
    fn sparse_bounds_match_dense() {
        let mut tmp = vec![0i32; 32 * 32];
        let mut st = 0x1234_5678u32;
        let mut rnd = |s: &mut u32| {
            *s = s.wrapping_mul(1_664_525).wrapping_add(1_013_904_223);
            ((*s >> 16) as i32 & 0x1ff) - 256
        };
        for &n in &[4usize, 8, 16, 32] {
            for &(nz_w, nz_h) in &[(1usize, 1usize), (2, 3), (4, 4), (n / 2, n / 4), (n, n)] {
                let (nz_w, nz_h) = (nz_w.min(n).max(1), nz_h.min(n).max(1));
                let mut dense = vec![0i32; n * n];
                for y in 0..nz_h {
                    for x in 0..nz_w {
                        dense[y * n + x] = rnd(&mut st);
                    }
                }
                let mut sparse = dense.clone();
                for &kind in &[TransformKind::Dct, TransformKind::Dst] {
                    if kind == TransformKind::Dst && n != 4 {
                        continue;
                    }
                    let mut a = dense.clone();
                    let mut b = sparse.clone();
                    inverse_transform(&mut a, &mut tmp, n, n, n, 8, kind);
                    inverse_transform(&mut b, &mut tmp, n, nz_w, nz_h, 8, kind);
                    assert_eq!(a, b, "n={n} nz=({nz_w},{nz_h}) {kind:?}");
                }
                sparse[0] = dense[0];
            }
        }
    }

    #[test]
    fn dequant_flat_qp() {
        let mut c = vec![0i32; 16];
        c[0] = 10;
        dequant(&mut c, 4, 4, 4, 4, 8, None);
        assert_eq!(c[0], 320);
    }

    /// Scaling the non-zero rectangle must equal scaling the whole block.
    #[test]
    fn dequant_bounds_match_dense() {
        let mut a = vec![0i32; 64];
        let mut b = vec![0i32; 64];
        for (i, (x, y)) in a.iter_mut().zip(b.iter_mut()).enumerate() {
            if i % 8 < 3 && i / 8 < 2 {
                *x = (i as i32) - 30;
                *y = *x;
            }
        }
        dequant(&mut a, 8, 8, 8, 17, 8, None);
        dequant(&mut b, 8, 3, 2, 17, 8, None);
        assert_eq!(a, b);
    }

    #[test]
    fn transform_skip_scales() {
        let mut tmp = vec![0i32; 32 * 32];
        let mut d = vec![0i32; 16];
        d[5] = 3;
        inverse_transform(&mut d, &mut tmp, 4, 4, 4, 8, TransformKind::Skip);
        assert_eq!(d[5], 0);
        let mut d = vec![0i32; 16];
        d[5] = 40;
        inverse_transform(&mut d, &mut tmp, 4, 4, 4, 8, TransformKind::Skip);
        assert_eq!(d[5], (40 * 128 + 2048) >> 12);
    }

    /// The algebraic property the partial butterfly rests on.
    ///
    /// HEVC's transform matrix is built so that even-indexed rows are
    /// symmetric about the midpoint and odd-indexed rows are antisymmetric:
    ///
    /// ```text
    ///   T[2m][j]   ==  T[2m][N-1-j]
    ///   T[2m+1][j] == -T[2m+1][N-1-j]
    /// ```
    ///
    /// which is what lets `out[j]` and `out[N-1-j]` be computed from one pair
    /// of partial sums instead of two independent dot products. If this ever
    /// stops holding, the butterfly is silently wrong and every other test
    /// still passes on the naive path — so it is asserted here directly.
    #[test]
    fn transform_matrix_has_the_butterfly_symmetry() {
        for &n in &[4usize, 8, 16, 32] {
            let step = 32 / n;
            for k in 0..n {
                for j in 0..n {
                    let a = DCT32[k * step][j];
                    let b = DCT32[k * step][n - 1 - j];
                    if k % 2 == 0 {
                        assert_eq!(a, b, "even row {k} of the {n}-point matrix is not symmetric at {j}");
                    } else {
                        assert_eq!(a, -b, "odd row {k} of the {n}-point matrix is not antisymmetric at {j}");
                    }
                }
            }
        }
    }

    /// The butterfly is a reassociation of an exact integer sum, so it must be
    /// bit-identical to the naive matrix product — not close, identical.
    #[test]
    fn butterfly_matches_naive() {
        let mut st = 0x51ee_7a11u32;
        let mut rnd = |s: &mut u32| {
            *s = s.wrapping_mul(1_664_525).wrapping_add(1_013_904_223);
            ((*s >> 8) as i32 % 65536) - 32768
        };
        for &n in &[4usize, 8, 16, 32] {
            for nz in 1..=n {
                for stride in [1usize, n, n + 3] {
                    let src: Vec<i32> = (0..n * stride + 8).map(|_| rnd(&mut st)).collect();
                    let mut a = vec![0i32; n];
                    let mut b = vec![0i32; n];
                    idct_sums_naive(&src, stride, n, nz, &mut a);
                    idct_sums(&src, stride, n, nz, &mut b);
                    assert_eq!(a, b, "n={n} nz={nz} stride={stride}");
                }
            }
        }
    }

    /// The DC-only collapse must reproduce the general path exactly.
    #[test]
    fn dc_only_matches_the_general_transform() {
        let mut st = 0x0dc0_0001u32;
        let mut tmp = vec![0i32; 32 * 32];
        for &bd in &[8u8, 10] {
            for &n in &[4usize, 8, 16, 32] {
                for _ in 0..32 {
                    st = st.wrapping_mul(1_664_525).wrapping_add(1_013_904_223);
                    let dc = ((st >> 8) as i32 % 65536) - 32768;
                    let mut want = vec![0i32; n * n];
                    let mut got = vec![0i32; n * n];
                    want[0] = dc;
                    got[0] = dc;
                    // Force the general path by claiming a 2x2 non-zero
                    // rectangle; the extra coefficients are zero, so the value
                    // is identical and only the ROUTE differs.
                    inverse_transform(&mut want, &mut tmp, n, 2.min(n), 2.min(n), bd, TransformKind::Dct);
                    inverse_transform(&mut got, &mut tmp, n, 1, 1, bd, TransformKind::Dct);
                    assert_eq!(want, got, "n={n} bd={bd} dc={dc}");
                }
            }
        }
    }

    /// The transform accumulator fits `i32`, with room to spare.
    ///
    /// It was written with `i64` accumulators and `as i64` on every operand,
    /// which is the safe default and costs real work: doubled register
    /// pressure, a widening conversion per coefficient, and — the expensive
    /// part — half the lanes in any vector form of the loop.
    ///
    /// The bound is not close. Coefficients are clipped to `COEFF_MIN..=COEFF_MAX`
    /// before the first pass and again between passes, and every output is
    /// `sum |c| * |T[k][j]|`, so the worst case is `32768 * max_j sum_k |T[k][j]|`.
    /// This computes that from the table itself rather than trusting a
    /// hand-derived number, so regenerating `DCT32` cannot silently invalidate it.
    #[test]
    fn transform_accumulator_fits_i32() {
        let worst_col = (0..32)
            .map(|j| (0..32).map(|k| (DCT32[k][j] as i64).abs()).sum::<i64>())
            .max()
            .unwrap();
        let worst = COEFF_MAX.max(-COEFF_MIN) as i64 * worst_col;
        assert!(
            worst <= i32::MAX as i64,
            "accumulator needs i64: worst case {worst} against {}",
            i32::MAX
        );
        // Record the margin, so a future table change that eats it fails here
        // rather than wrapping silently in the kernel.
        let headroom = i32::MAX as i64 / worst;
        assert!(headroom >= 4, "only {headroom}x headroom left; re-derive before narrowing further");
        eprintln!("transform accumulator: worst {worst}, headroom {headroom}x");
    }
}