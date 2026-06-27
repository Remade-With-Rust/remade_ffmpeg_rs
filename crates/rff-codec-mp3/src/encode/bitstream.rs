//! Frame assembly + the encoder-side bit reservoir.
//!
//! Writes the header (+ optional CRC), the side-information block, and the main
//! data. The reservoir lets a granule borrow unused bits donated by earlier
//! frames: `main_data_begin` records how far back this frame's main data starts,
//! so a complex granule can spend more than its nominal budget.

use crate::frame::SideInfo;
use crate::header::FrameHeader;

/// Encoder-side reservoir: how many spare main-data bytes are banked.
#[derive(Debug, Clone, Default)]
pub struct EncReservoir {
    /// Spare bytes carried forward for future frames to borrow.
    pub spare_bytes: usize,
}

/// Assemble one complete MP3 frame: header + CRC + side info + main data, with
/// `main_data_begin` set from the reservoir state (which this updates).
pub fn format(
    _header: &FrameHeader,
    _side_info: &SideInfo,
    _main_data: &[u8],
    _reservoir: &mut EncReservoir,
) -> Vec<u8> {
    // brick: emit FrameHeader::to_bytes; serialize SideInfo (set main_data_begin
    // from spare_bytes); pad main_data to the frame size; update spare_bytes with
    // the leftover. Optional CRC-16 over header+side-info.
    todo!("mp3 encode: frame assembly")
}
