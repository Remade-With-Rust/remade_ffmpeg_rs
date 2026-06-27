//! Joint-stereo reconstruction: MS (mid/side) and intensity stereo.
//!
//! Only active when the header `mode` is JointStereo; `mode_extension` says which
//! of MS / intensity is on. MS rotates (mid,side)→(L,R) by `1/√2`. Intensity
//! stereo, above the intensity bound, reconstructs R from L using a per-band
//! position (the band's "scalefactor" carries the pan, MPEG-1 vs MPEG-2 differ).

use crate::frame::{GranuleSideInfo, GranuleSpectrum};
use crate::header::FrameHeader;

/// Convert the two coded channels in place from the joint representation back to
/// independent left/right.
pub fn process(
    _header: &FrameHeader,
    _gi: &[GranuleSideInfo; 2],
    _spectrum: &mut GranuleSpectrum,
) {
    // brick: if intensity on, find the intensity bound (first zero band of the
    // right channel) and pan; if MS on, apply the 1/√2 mid/side butterfly to the
    // non-intensity region. No-op for plain stereo / mono.
    todo!("mp3 decode: joint-stereo")
}
