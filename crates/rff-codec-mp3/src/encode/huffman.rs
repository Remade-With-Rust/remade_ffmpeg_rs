//! Huffman encoding — the inverse of decode's spectrum decode.
//!
//! Partitions the quantized spectrum into big-values regions + the count1 quad
//! region, picks the cheapest Huffman table per region (this choice feeds back
//! into the quantizer's bit-cost estimate), and writes the codewords with
//! `linbits` escapes and sign bits.

use crate::bitio::BitWriter;

use super::quantize::QuantizedGranule;

/// Encode one quantized granule's spectrum into `writer`.
pub fn encode(_quant: &QuantizedGranule, _writer: &mut BitWriter) {
    // brick: from region boundaries, encode big_values pairs via the selected
    // tables (+linbits + signs), then count1 quads. Table selection should match
    // the cost model the quantizer used.
    todo!("mp3 encode: Huffman spectrum encode")
}

/// Estimate the bit cost of coding `coeffs` with a given table — used by the
/// quantizer's inner loop without actually emitting bits.
pub fn estimate_bits(_coeffs: &[i32], _table: u8) -> usize {
    // brick: sum codeword lengths (+linbits +signs) for the region under `table`.
    todo!("mp3 encode: Huffman bit-cost estimate")
}
