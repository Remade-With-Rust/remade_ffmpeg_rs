//! Forward MDCT — turns each subband's time samples into 18 frequency lines.
//!
//! One 36-point MDCT per subband for long blocks (windowed Long/Start/Stop), or
//! three 12-point MDCTs for short blocks. Overlap is carried from the previous
//! granule. Mixed blocks keep subbands 0-1 long. Output is subband-major,
//! matching what requantize/Huffman expect.

use crate::frame::{BlockType, GRANULE_LINES, SUBBANDS, SUBBAND_LINES};

/// Forward-transform one channel's granule. `overlap` carries the previous
/// granule's tail in and this granule's tail out. Returns 576 frequency lines.
pub fn forward(
    _subbands: &[[f32; SUBBAND_LINES]; SUBBANDS],
    _block_type: BlockType,
    _overlap: &mut [f32; GRANULE_LINES],
) -> [f32; GRANULE_LINES] {
    // brick: per subband window per block_type, run the 36- or 3×12-point MDCT,
    // overlap-add the previous tail, store the new tail. Frequency-invert odd
    // subbands to match the analysis filterbank's sign convention.
    todo!("mp3 encode: forward MDCT")
}
