//! Huffman decoding of the quantized spectrum.
//!
//! The `big_values` region splits into up to three sub-regions, each using one of
//! the 32 Layer III pair-tables (with `linbits` escapes for large magnitudes).
//! After it, the `count1` region uses one of two quad-tables (4 values per code,
//! each ±1/0). The rest of the 576 lines are implicit zeros.

use crate::frame::{GranuleSideInfo, GRANULE_LINES};
use crate::header::FrameHeader;

/// Decode one granule/channel's quantized coefficients, advancing `bit_pos`
/// (stopping at the granule's `part2_3_length`). Returns the integer
/// coefficients and the count of non-zero lines (the rzero boundary).
pub fn decode(
    _main: &[u8],
    _bit_pos: &mut usize,
    _header: &FrameHeader,
    _gi: &GranuleSideInfo,
) -> ([i32; GRANULE_LINES], usize) {
    // brick: resolve region boundaries from region0/1_count + the sfb table;
    // decode big_values pairs via the selected tables (+linbits + sign), then
    // count1 quads; honor the part2_3_length bit budget. For short blocks the
    // region split is fixed.
    todo!("mp3 decode: Huffman spectrum decode")
}
