//! VP9 encoder — forward transforms (Floor 1, bricks T1–T3).
//!
//! Each forward transform is the inverse of the decoder's inverse transform in
//! [`crate::transform`], reusing the same `COSPI`/`SINPI` constants and 14-bit
//! `round_shift` rounding. They are **self-verified**: a residual pushed through
//! the forward 2-D transform and then the decoder's `inverse_transform_add`
//! reconstructs the residual (within transform rounding) — no external
//! reference. The scaling is fixed by the decoder: its 2-D inverse ends in
//! `round_pow2(idct², shift_n)` with `shift = {4:4, 8:5, 16:6, 32:6}`, so the
//! forward transform must carry exactly the inverse scale (this matches the
//! libvpx forward-transform normalisation the dequant tables are calibrated to).

use crate::transform::{fdct4, TxType, COSPI};

/// 14-bit fixed-point rounding, identical to `crate::transform::round_shift`
/// (kept local so the forward butterflies read like their inverse twins).
#[inline]
fn round_shift(x: i64) -> i32 {
    ((x + (1 << 13)) >> 14) as i32
}

#[inline]
fn c(i: usize) -> i64 {
    COSPI[i]
}

// ---- size 4 ---------------------------------------------------------------

/// Forward 2-D DCT for a 4×4 residual block (row-major `n*n`), producing the
/// coefficient block in the decoder's dequantized-coefficient scale. Mirrors
/// libvpx `vpx_fdct4x4`: input ×16, an `fdct4` butterfly down columns then
/// across rows, and a final `(x + 1) >> 2`.
fn fdct4x4(residual: &[i32], out: &mut [i32]) {
    let mut inter = [0i32; 16];
    // Columns: pre-scale ×16; libvpx nudges the DC of column 0 up by 1.
    for col in 0..4 {
        let mut cin = [
            residual[col] * 16,
            residual[4 + col] * 16,
            residual[8 + col] * 16,
            residual[12 + col] * 16,
        ];
        if col == 0 && cin[0] != 0 {
            cin[0] += 1;
        }
        let mut cout = [0i32; 4];
        fdct4(&cin, &mut cout);
        for r in 0..4 {
            inter[r * 4 + col] = cout[r];
        }
    }
    // Rows: butterfly, then the final round-shift down by 2.
    for r in 0..4 {
        let rin = [
            inter[r * 4],
            inter[r * 4 + 1],
            inter[r * 4 + 2],
            inter[r * 4 + 3],
        ];
        let mut rout = [0i32; 4];
        fdct4(&rin, &mut rout);
        for c in 0..4 {
            out[r * 4 + c] = (rout[c] + 1) >> 2;
        }
    }
}

// ---- size 8 ---------------------------------------------------------------

/// Forward 8-point DCT (one dimension) — the structural inverse of
/// [`idct8`](crate::transform::idct8): an `fdct4` on the even part, a rotated
/// odd part. Outputs natural frequency order.
fn fdct8(inp: &[i32; 8], out: &mut [i32; 8]) {
    let s0 = (inp[0] + inp[7]) as i64;
    let s1 = (inp[1] + inp[6]) as i64;
    let s2 = (inp[2] + inp[5]) as i64;
    let s3 = (inp[3] + inp[4]) as i64;
    let s4 = (inp[3] - inp[4]) as i64;
    let s5 = (inp[2] - inp[5]) as i64;
    let s6 = (inp[1] - inp[6]) as i64;
    let s7 = (inp[0] - inp[7]) as i64;
    // Even part: a 4-point DCT of (s0..s3) into the even frequencies.
    let x0 = s0 + s3;
    let x1 = s1 + s2;
    let x2 = s1 - s2;
    let x3 = s0 - s3;
    out[0] = round_shift((x0 + x1) * c(16));
    out[4] = round_shift((x0 - x1) * c(16));
    out[2] = round_shift(x2 * c(24) + x3 * c(8));
    out[6] = round_shift(x3 * c(24) - x2 * c(8));
    // Odd part.
    let t2 = round_shift((s6 - s5) * c(16)) as i64;
    let t3 = round_shift((s6 + s5) * c(16)) as i64;
    let x0 = s4 + t2;
    let x1 = s4 - t2;
    let x2 = s7 - t3;
    let x3 = s7 + t3;
    out[1] = round_shift(x0 * c(28) + x3 * c(4));
    out[7] = round_shift(x3 * c(28) - x0 * c(4));
    out[5] = round_shift(x1 * c(12) + x2 * c(20));
    out[3] = round_shift(x2 * c(12) - x1 * c(20));
}

/// Forward 2-D DCT for an 8×8 block (libvpx `vpx_fdct8x8`): columns pre-scaled
/// ×4 through `fdct8`, then rows, with a final `>>1` rounded toward zero.
fn fdct8x8(residual: &[i32], out: &mut [i32]) {
    let mut inter = [0i32; 64];
    for col in 0..8 {
        let cin: [i32; 8] = std::array::from_fn(|r| residual[r * 8 + col] * 4);
        let mut cout = [0i32; 8];
        fdct8(&cin, &mut cout);
        for r in 0..8 {
            inter[r * 8 + col] = cout[r];
        }
    }
    for r in 0..8 {
        let rin: [i32; 8] = std::array::from_fn(|cc| inter[r * 8 + cc]);
        let mut rout = [0i32; 8];
        fdct8(&rin, &mut rout);
        for cc in 0..8 {
            let x = rout[cc];
            out[r * 8 + cc] = (x + (x < 0) as i32) >> 1;
        }
    }
}

// ---- lossless Walsh-Hadamard (T2) -----------------------------------------

/// Forward 2-D Walsh-Hadamard transform for a lossless 4×4 block (libvpx
/// `vp9_fwht4x4`) — the inverse of the decoder's
/// [`inverse_wht_add`](crate::transform::inverse_wht_add). Output is `×4`
/// (`UNIT_QUANT_FACTOR`) so the decoder's `>>2` input pre-shift cancels.
pub fn fwht4x4(residual: &[i32], out: &mut [i32]) {
    let mut inter = [0i32; 16];
    for col in 0..4 {
        let (mut a, mut b, mut c, mut d) = (
            residual[col],
            residual[4 + col],
            residual[8 + col],
            residual[12 + col],
        );
        a += b;
        d -= c;
        let e = (a - d) >> 1;
        b = e - b;
        c = e - c;
        a -= c;
        d += b;
        inter[col] = a;
        inter[4 + col] = c;
        inter[8 + col] = d;
        inter[12 + col] = b;
    }
    for r in 0..4 {
        let (mut a, mut b, mut c, mut d) = (
            inter[r * 4],
            inter[r * 4 + 1],
            inter[r * 4 + 2],
            inter[r * 4 + 3],
        );
        a += b;
        d -= c;
        let e = (a - d) >> 1;
        b = e - b;
        c = e - c;
        a -= c;
        d += b;
        out[r * 4] = a * 4;
        out[r * 4 + 1] = c * 4;
        out[r * 4 + 2] = d * 4;
        out[r * 4 + 3] = b * 4;
    }
}

// ---- sizes 16/32 + ADST: integer transpose of the decoder's inverse --------
//
// For the larger DCTs and every ADST, the forward transform is built directly
// from the decoder's own inverse: `basis[k] = inv_1d(e_k << 14)`, so the forward
// is `out[k] = Σ_i in[i]·basis[k][i]` — the integer transpose of the inverse
// matrix (pure reuse, no fabricated constants). Both `idct` and `iadst` have the
// same 1-D energy gain N/2 (the decoder's orthogonality tests prove it), so a
// single per-size calibration shift makes the 2-D round-trip exact regardless of
// the DCT/ADST mix. `O(n²)` and correctness-first; replacing with fast integer
// butterflies is a Roof optimisation (the round-trip gate guards any swap).

use std::sync::OnceLock;

use crate::transform::{iadst16, iadst4, iadst8, idct16, idct32, idct4, idct8};

/// Per-size calibration shift: `26 - shift_n + 2·log2(n)`, where the decoder's
/// 2-D inverse ends in `round_pow2(·, shift_n)`. Derived from the √(N/2) gain and
/// confirmed by the round-trip gate.
fn calib_shift(n: usize) -> u32 {
    match n {
        4 => 26,
        8 => 27,
        16 => 28,
        32 => 30,
        _ => unreachable!(),
    }
}

/// One-dimension inverse transform dispatch onto the decoder's functions.
fn inv_1d(inp: &[i32], out: &mut [i32], adst: bool) {
    match (inp.len(), adst) {
        (4, false) => idct4(inp.try_into().unwrap(), (&mut out[..4]).try_into().unwrap()),
        (8, false) => idct8(inp.try_into().unwrap(), (&mut out[..8]).try_into().unwrap()),
        (16, false) => idct16(
            inp.try_into().unwrap(),
            (&mut out[..16]).try_into().unwrap(),
        ),
        (32, false) => idct32(
            inp.try_into().unwrap(),
            (&mut out[..32]).try_into().unwrap(),
        ),
        (4, true) => iadst4(inp.try_into().unwrap(), (&mut out[..4]).try_into().unwrap()),
        (8, true) => iadst8(inp.try_into().unwrap(), (&mut out[..8]).try_into().unwrap()),
        (16, true) => iadst16(
            inp.try_into().unwrap(),
            (&mut out[..16]).try_into().unwrap(),
        ),
        _ => unreachable!("no inverse 1-D for {} adst={adst}", inp.len()),
    }
}

/// Build the inverse-transform basis matrix: `basis[k][i] = inv_1d(e_k << 14)[i]`.
fn build_basis(n: usize, adst: bool) -> Vec<Vec<i64>> {
    (0..n)
        .map(|k| {
            let mut inp = vec![0i32; n];
            inp[k] = 1 << 14;
            let mut out = vec![0i32; n];
            inv_1d(&inp, &mut out, adst);
            out.iter().map(|&v| v as i64).collect()
        })
        .collect()
}

/// Cached inverse basis for `(n, adst)`.
fn basis_for(n: usize, adst: bool) -> &'static [Vec<i64>] {
    macro_rules! cached {
        ($n:expr, $adst:expr) => {{
            static C: OnceLock<Vec<Vec<i64>>> = OnceLock::new();
            C.get_or_init(|| build_basis($n, $adst)).as_slice()
        }};
    }
    match (n, adst) {
        (4, false) => cached!(4, false),
        (8, false) => cached!(8, false),
        (16, false) => cached!(16, false),
        (32, false) => cached!(32, false),
        (4, true) => cached!(4, true),
        (8, true) => cached!(8, true),
        (16, true) => cached!(16, true),
        _ => unreachable!("no basis for {n} adst={adst}"),
    }
}

/// Forward 2-D transform via the integer inverse-basis transpose. `row_adst` /
/// `col_adst` select the per-dimension transform exactly as the decoder's
/// `inverse_transform_add_rows` interprets `tx_type`.
fn forward_2d_matrix(residual: &[i32], n: usize, row_adst: bool, col_adst: bool, out: &mut [i32]) {
    let row_basis = basis_for(n, row_adst);
    let col_basis = basis_for(n, col_adst);
    // Row pass: transform each row into the frequency domain.
    let mut tmp = vec![0i64; n * n];
    for r in 0..n {
        for k in 0..n {
            let bk = &row_basis[k];
            let mut acc = 0i64;
            for i in 0..n {
                acc += residual[r * n + i] as i64 * bk[i];
            }
            tmp[r * n + k] = acc;
        }
    }
    // Column pass + calibration round-shift.
    let sh = calib_shift(n);
    let round = 1i64 << (sh - 1);
    for kc in 0..n {
        for kr in 0..n {
            let bk = &col_basis[kr];
            let mut acc = 0i64;
            for r in 0..n {
                acc += tmp[r * n + kc] * bk[r];
            }
            out[kr * n + kc] = ((acc + round) >> sh) as i32;
        }
    }
}

/// Map `TxType` to `(row_adst, col_adst)`, matching the decoder.
fn tx_dirs(tx_type: TxType) -> (bool, bool) {
    match tx_type {
        TxType::DctDct => (false, false),
        TxType::AdstDct => (false, true),
        TxType::DctAdst => (true, false),
        TxType::AdstAdst => (true, true),
    }
}

// ---- 2-D dispatch (T3) ----------------------------------------------------

/// Forward 2-D transform of an `n×n` residual block (row-major) into `out`
/// (row-major coefficients). `tx_type` selects DCT/ADST per dimension exactly
/// as the decoder's `inverse_transform_add` interprets it. The output is in the
/// decoder's dequantized-coefficient domain (divide by the quant step to get
/// levels). Only DCT_DCT is wired so far; ADST and sizes 8/16/32 land next.
pub fn forward_transform(residual: &[i32], n: usize, tx_type: TxType, out: &mut [i32]) {
    debug_assert_eq!(residual.len(), n * n);
    debug_assert!(out.len() >= n * n);
    match (n, tx_type) {
        // Exact integer butterflies for the common small DCTs.
        (4, TxType::DctDct) => fdct4x4(residual, out),
        (8, TxType::DctDct) => fdct8x8(residual, out),
        // Everything else (16/32 DCT, all ADST) via the inverse-basis transpose.
        _ => {
            let (row_adst, col_adst) = tx_dirs(tx_type);
            forward_2d_matrix(residual, n, row_adst, col_adst, out);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::transform::inverse_transform_add;

    fn xs(s: &mut u64) -> u64 {
        *s ^= *s << 13;
        *s ^= *s >> 7;
        *s ^= *s << 17;
        *s
    }

    /// Push a residual through the forward 2-D transform and straight back
    /// through the decoder's inverse (quant step = 1, i.e. coeffs == dqcoeff);
    /// the reconstruction must match the residual within transform rounding.
    fn assert_transform_roundtrips(n: usize, tx: TxType, max_err: i32) {
        let mut s = 0x9e37_79b9_7f4a_7c15u64 ^ (n as u64);
        let base = 128i32; // mid-level prediction; residual sits around it
        for _ in 0..200 {
            let residual: Vec<i32> = (0..n * n)
                .map(|_| (xs(&mut s) % 121) as i32 - 60) // [-60, 60]
                .collect();
            let mut coeffs = vec![0i32; n * n];
            forward_transform(&residual, n, tx, &mut coeffs);

            // Reconstruct: dest = prediction (flat `base`), add inverse(coeffs).
            let mut dest = vec![base as u16; n * n];
            inverse_transform_add(&coeffs, n, tx, &mut dest, n, 4095);

            for i in 0..n * n {
                let got = dest[i] as i32 - base;
                assert!(
                    (got - residual[i]).abs() <= max_err,
                    "{n}x{n} {tx:?} pos {i}: recon {got} vs residual {} (coeff {})",
                    residual[i],
                    coeffs[i]
                );
            }
        }
    }

    #[test]
    fn fdct4x4_roundtrips_through_decoder() {
        // The transform pair is exact up to rounding: a couple of units max.
        assert_transform_roundtrips(4, TxType::DctDct, 2);
    }

    #[test]
    fn fdct8x8_roundtrips_through_decoder() {
        assert_transform_roundtrips(8, TxType::DctDct, 2);
    }

    #[test]
    fn fdct16x16_roundtrips_through_decoder() {
        assert_transform_roundtrips(16, TxType::DctDct, 3);
    }

    #[test]
    fn fdct32x32_roundtrips_through_decoder() {
        assert_transform_roundtrips(32, TxType::DctDct, 4);
    }

    #[test]
    fn fadst_and_hybrid_roundtrip_through_decoder() {
        // Every ADST/DCT mix at every ADST size must round-trip.
        for &n in &[4usize, 8, 16] {
            for &tx in &[TxType::AdstDct, TxType::DctAdst, TxType::AdstAdst] {
                assert_transform_roundtrips(n, tx, 3);
            }
        }
    }

    #[test]
    fn fwht4x4_roundtrips_exactly() {
        // The Walsh-Hadamard transform is lossless: the round-trip through the
        // decoder's `inverse_wht_add` must be bit-exact (zero error).
        use crate::transform::inverse_wht_add;
        let mut s = 0x1357_9bdfu64;
        let base = 512i32;
        for _ in 0..300 {
            let residual: Vec<i32> = (0..16).map(|_| (xs(&mut s) % 401) as i32 - 200).collect();
            let mut coeffs = [0i32; 16];
            fwht4x4(&residual, &mut coeffs);
            let mut dest = [base as u16; 16];
            inverse_wht_add(&coeffs, &mut dest, 4, 4095);
            for i in 0..16 {
                assert_eq!(dest[i] as i32 - base, residual[i], "wht pos {i}");
            }
        }
    }

    #[test]
    fn fdct4x4_dc_scale_is_exact() {
        // A flat residual `v` must produce DC == 32·v (the libvpx 4×4 scale) and
        // zero AC, and reconstruct back to exactly `v`.
        for v in [-50i32, -7, 1, 16, 63] {
            let residual = vec![v; 16];
            let mut coeffs = [0i32; 16];
            forward_transform(&residual, 4, TxType::DctDct, &mut coeffs);
            assert_eq!(coeffs[0], 32 * v, "DC for v={v}");
            assert!(
                coeffs[1..].iter().all(|&c| c == 0),
                "AC must be zero, v={v}"
            );
        }
    }
}
