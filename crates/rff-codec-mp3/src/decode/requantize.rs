//! Requantization: integer coefficients → dequantized frequency lines.
//!
//! Per line: `xr = sign(is) * |is|^(4/3) * 2^(0.25 * A) * 2^(-B)` where `A`
//! folds in `global_gain` (and `subblock_gain` for short blocks) and `B` folds
//! in the band's scalefactor scaled by `scalefac_scale` (and `preflag`). The
//! `|is|^(4/3)` term comes from the `POW43` table; short blocks then need the
//! reorder map applied (subband→scalefactor-band order).

use crate::frame::{GranuleSideInfo, GRANULE_LINES};
use crate::header::FrameHeader;

use super::scalefactors::ScaleFactors;

/// Dequantize `coeffs[..nz]` into `out`, applying gains, scalefactors, and (for
/// short blocks) the reorder.
pub fn apply(
    _header: &FrameHeader,
    _gi: &GranuleSideInfo,
    _sf: &ScaleFactors,
    _coeffs: &[i32; GRANULE_LINES],
    _nz: usize,
    _out: &mut [f32; GRANULE_LINES],
) {
    // brick: per scalefactor band, compute the exponent from global_gain,
    // scalefac_scale, preflag/pretab, subblock_gain; multiply by POW43[|is|];
    // then reorder short-block lines from subband order to sfb order.
    todo!("mp3 decode: requantize")
}
