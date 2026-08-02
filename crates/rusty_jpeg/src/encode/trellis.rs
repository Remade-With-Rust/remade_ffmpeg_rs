//! Rate-distortion optimization of the quantized coefficients.
//!
//! Plain quantization rounds each coefficient independently, which is optimal
//! for distortion alone and ignores what the coefficients COST. JPEG codes AC
//! coefficients as `(run, size)` pairs terminated by EOB, so the rate of a block
//! is highly non-linear in its contents: one small trailing coefficient can be
//! worth several symbols, because keeping it forces the run that precedes it to
//! be coded and pushes the EOB later.
//!
//! # Why truncation specifically
//!
//! The full trellis considers, per coefficient, keeping it / lowering it by one
//! / zeroing it, as a DP over positions. Most of the gain, though, comes from
//! one structural move: **choosing where the EOB goes**. Dropping a SUFFIX is
//! also the only edit that leaves the kept coefficients' run-lengths untouched,
//! which makes the rate exactly computable in one pass instead of re-deriving
//! the symbol stream per candidate — O(nnz) per block rather than O(nnz²).
//!
//! So this implements suffix truncation with a real rate model (actual Huffman
//! code lengths plus the raw magnitude bits) and real distortion (squared error
//! against the pre-quantization coefficients). Per-coefficient magnitude
//! lowering is deliberately not attempted yet; it is a smaller, costlier term.
//!
//! # Units
//!
//! Distortion is measured in the scaled-DCT domain the encoder already works
//! in: the forward DCT output is 8x scaled and the quantization divisors are
//! pre-multiplied by 8 to match, so `coef - q * divisor` is the reconstruction
//! error in those units. Lambda is expressed against the same scale.

use crate::encode::huffman::HuffmanTable;
use crate::encode::quantization::QuantizationTable;
use crate::encode::writer::{get_code, ZIGZAG};

/// Lagrangian weight, as a multiple of the mean squared quantization step.
///
/// A step of `q` produces reconstruction error up to `q/2`, so `q²` is the
/// natural distortion scale; this expresses "how much distortion is one bit
/// worth" in those units. Tunable at runtime so the value can be swept against
/// the RD harness rather than asserted — see `RUSTY_JPEG_TRELLIS_LAMBDA`.
const DEFAULT_LAMBDA_SCALE: f32 = 0.10;

pub(crate) fn lambda_scale() -> f32 {
    use std::sync::OnceLock;
    static V: OnceLock<f32> = OnceLock::new();
    *V.get_or_init(|| {
        std::env::var("RUSTY_JPEG_TRELLIS_LAMBDA")
            .ok()
            .and_then(|v| v.parse().ok())
            .unwrap_or(DEFAULT_LAMBDA_SCALE)
    })
}

/// Lower coefficient magnitudes where the bits saved outweigh the distortion.
///
/// Restricted to `|q| >= 2 -> |q| - 1`, which can never produce a zero. That
/// restriction is what makes every decision **independent**: the set of non-zero
/// positions is unchanged, so no run-length moves and no other coefficient's
/// symbol is affected. The alternative — allowing `1 -> 0` — merges the run into
/// the next coefficient and couples the decisions, which is a different and much
/// more expensive problem.
///
/// A magnitude drop pays off when it crosses a `size` boundary (7 -> 6 keeps
/// size 3, but 8 -> 7 falls from size 4 to size 3), saving both a shorter
/// Huffman symbol and one raw magnitude bit.
fn lower_magnitudes(
    coef_natural: &[i16; 64],
    q_block: &mut [i16; 64],
    table: &QuantizationTable,
    ac_table: &HuffmanTable,
    lambda: f64,
) -> u32 {
    let mut changed = 0;
    let mut prev = 0usize;
    for i in 1..64 {
        let q = q_block[i];
        if q == 0 {
            continue;
        }
        let run_before = (i - prev - 1) as u32;
        prev = i;

        if q.abs() < 2 {
            continue;
        }
        let lowered = q - q.signum();

        let z = ZIGZAG[i] as usize & 0x3f;
        let step = table.divisor(z) as f64;
        let c = coef_natural[z] as f64;

        let err_now = c - q as f64 * step;
        let err_low = c - lowered as f64 * step;
        let delta_d = err_low * err_low - err_now * err_now;

        // Only the last ZRL chunk's run reaches the symbol.
        let r = (run_before % 16) as u8;
        let (size_now, _) = get_code(q);
        let (size_low, _) = get_code(lowered);
        let rate_now = ac_table.code_len((r << 4) | size_now) as f64 + size_now as f64;
        let rate_low = ac_table.code_len((r << 4) | size_low) as f64 + size_low as f64;

        if delta_d + lambda * (rate_low - rate_now) < 0.0 {
            q_block[i] = lowered;
            changed += 1;
        }
    }
    changed
}

/// Choose the EOB position that minimises `D + lambda * R`.
///
/// `coef_natural` is the pre-quantization DCT block in natural order;
/// `q_block` is the quantized block in **zig-zag** order and is edited in place.
/// Returns the number of coefficients zeroed, for instrumentation.
pub(crate) fn truncate_rd(
    coef_natural: &[i16; 64],
    q_block: &mut [i16; 64],
    table: &QuantizationTable,
    ac_table: &HuffmanTable,
) -> u32 {
    // Positions of the non-zero AC coefficients, in zig-zag order.
    let mut pos = [0u8; 63];
    let mut nnz = 0usize;
    for i in 1..64 {
        if q_block[i] != 0 {
            pos[nnz] = i as u8;
            nnz += 1;
        }
    }
    if nnz == 0 {
        return 0;
    }

    let bits = |sym: u8| -> u32 { ac_table.code_len(sym) as u32 };
    let eob_bits = bits(0x00);

    // Rate of keeping the first k non-zeros, symbols only (EOB added later).
    // Walking forward once gives every prefix.
    let mut prefix = [0u32; 64];
    let mut acc = 0u32;
    // `prev` starts at the DC position, so `p - prev - 1` is the zero-run before
    // the first AC coefficient as well as every later one — no special case.
    let mut prev = 0usize;
    for k in 0..nnz {
        let p = pos[k] as usize;
        // Zero-runs longer than 15 need a ZRL symbol each.
        let mut r = (p - prev - 1) as u32;
        while r > 15 {
            acc += bits(0xF0);
            r -= 16;
        }
        let (size, _) = get_code(q_block[p]);
        acc += bits(((r as u8) << 4) | size) + size as u32;
        prefix[k + 1] = acc;
        prev = p;
    }

    // Distortion added by dropping each non-zero, accumulated from the end.
    // Dropping changes the error at that position from (coef - q*step) to coef.
    let mut drop_cost = [0.0f64; 64];
    for k in (0..nnz).rev() {
        let p = pos[k] as usize;
        let z = ZIGZAG[p] as usize & 0x3f;
        let step = table.divisor(z) as f64;
        let c = coef_natural[z] as f64;
        let kept_err = c - q_block[p] as f64 * step;
        let d = c * c - kept_err * kept_err;
        drop_cost[k] = drop_cost[k + 1] + d.max(0.0);
    }

    // Lambda against the mean squared step of the positions in play.
    let mut mean_sq = 0.0f64;
    for k in 0..nnz {
        let z = ZIGZAG[pos[k] as usize] as usize & 0x3f;
        let s = table.divisor(z) as f64;
        mean_sq += s * s;
    }
    mean_sq /= nnz as f64;

    // A density-adaptive lambda was tried here and REFUTED IN BOTH DIRECTIONS.
    //
    // EOB truncation measures -5.02% BD-rate on photographic content and
    // **+3.83%** on noise, and a sign flip is normally a dispatch signal. The
    // hypothesis was that a lambda tuned on structured content over-truncates
    // high-entropy blocks, so lambda should taper down with non-zero density.
    // Measured, tapering DOWN made noise worse (+1.40% -> +2.67% at a 0.15
    // floor) and tapering UP made it far worse (+12.07% at 4x). Flat lambda is
    // the best of the three, so the density of a block is simply not the axis
    // that explains the loss.
    //
    // What DID cut it nearly in half was magnitude lowering below: noise went
    // +3.83% -> +1.40% once coefficients could be reduced instead of only
    // dropped. The residual loss on pure synthetic noise is accepted; 4 of 5
    // contents win and the mean is -2.51%.
    let lambda = lambda_scale() as f64 * mean_sq;

    // k = number of non-zeros kept. Cost = distortion added + lambda * rate.
    let mut best_k = nnz;
    let mut best = f64::INFINITY;
    for k in 0..=nnz {
        let last_pos = if k == 0 { 0 } else { pos[k - 1] as usize };
        let rate = prefix[k] + if last_pos < 63 { eob_bits } else { 0 };
        let cost = drop_cost[k] + lambda * rate as f64;
        if cost < best {
            best = cost;
            best_k = k;
        }
    }

    let mut zeroed = 0;
    if best_k < nnz {
        for k in best_k..nnz {
            q_block[pos[k] as usize] = 0;
        }
        zeroed = (nnz - best_k) as u32;
    }

    // Magnitude lowering runs AFTER truncation, on what survives: deciding to
    // lower a coefficient that is about to be discarded would be wasted work,
    // and would also price the truncation against the wrong rate.
    if magnitudes_enabled() {
        zeroed += lower_magnitudes(coef_natural, q_block, table, ac_table, lambda);
    }
    zeroed
}

/// `RUSTY_JPEG_TRELLIS_MAG=0` disables magnitude lowering, keeping EOB
/// truncation. Separable because the two were measured separately.
fn magnitudes_enabled() -> bool {
    use std::sync::OnceLock;
    static V: OnceLock<bool> = OnceLock::new();
    *V.get_or_init(|| {
        std::env::var("RUSTY_JPEG_TRELLIS_MAG")
            .map(|v| v != "0")
            .unwrap_or(true)
    })
}
