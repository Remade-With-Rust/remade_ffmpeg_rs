//! Scalefactor decoding.
//!
//! Each scalefactor band gets a small integer that scales its requantization
//! step. Bit-lengths come from `scalefac_compress` (slen1/slen2); for MPEG-1 the
//! `scfsi` flags let granule 1 reuse granule 0's scalefactors per band group.
//! Short blocks store three sets (one per window).

use crate::frame::SideInfo;
use crate::header::FrameHeader;

/// Decoded scalefactors for one granule/channel.
#[derive(Debug, Clone, Default)]
pub struct ScaleFactors {
    /// Long-block bands `[0..22)`.
    pub long: [u8; crate::frame::SFB_LONG],
    /// Short-block bands `[window][0..13)`.
    pub short: [[u8; crate::frame::SFB_SHORT]; 3],
}

/// Read one granule/channel's scalefactors from the main-data bitstream,
/// advancing `bit_pos`.
pub fn decode(
    _main: &[u8],
    _bit_pos: &mut usize,
    _header: &FrameHeader,
    _si: &SideInfo,
    _gr: usize,
    _ch: usize,
) -> ScaleFactors {
    // brick: split scalefac_compress → (slen1, slen2); read per-band values for
    // long/short/mixed blocks; apply scfsi reuse for MPEG-1 granule 1. Use the
    // MPEG-2 intensity-stereo scalefactor scheme when applicable.
    todo!("mp3 decode: scalefactor decode")
}
