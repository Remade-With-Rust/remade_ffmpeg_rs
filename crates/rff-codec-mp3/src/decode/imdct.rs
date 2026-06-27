//! Hybrid synthesis stage 1: the inverse MDCT with windowing and overlap-add.
//!
//! Each subband's 18 frequency lines run through an IMDCT — one 36-point
//! transform for long blocks, or three 12-point transforms for short blocks —
//! then one of the four windows (Long/Start/Short/Stop) is applied. The first
//! half overlaps the previous granule's stored tail; the second half is saved as
//! the next overlap. Odd subbands then get frequency inversion before synthesis.

use crate::frame::{GranuleSideInfo, GRANULE_LINES};

/// Run the hybrid IMDCT for one channel's granule. `overlap` holds the previous
/// granule's tail on entry and is updated with this granule's tail on exit.
/// Returns the 576 time-domain values (subband-major) for the synthesis stage.
pub fn hybrid(
    _gi: &GranuleSideInfo,
    _lines: &[f32; GRANULE_LINES],
    _overlap: &mut [f32; GRANULE_LINES],
) -> [f32; GRANULE_LINES] {
    // brick: per subband select block_type → IMDCT size + window; overlap-add the
    // first 18 with `overlap`, store the next 18; apply frequency inversion to odd
    // subbands. Mixed blocks: subbands 0-1 long, 2-31 short.
    todo!("mp3 decode: hybrid IMDCT")
}
