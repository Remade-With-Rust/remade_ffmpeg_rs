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

    if best_k == nnz {
        return 0;
    }
    for k in best_k..nnz {
        q_block[pos[k] as usize] = 0;
    }
    (nnz - best_k) as u32
}
