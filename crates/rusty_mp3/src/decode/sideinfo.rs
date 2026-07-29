//! Side-information parser (the bytes between the header/CRC and the main data).
//!
//! Lays out `main_data_begin`, `scfsi`, and per-granule/channel fields
//! (`part2_3_length`, `big_values`, `global_gain`, window switching, table
//! selects, region counts, flags). MPEG-1 carries 2 granules + `scfsi` and uses
//! 4-bit `scalefac_compress` + a `preflag` bit; MPEG-2/2.5 carries 1 granule, no
//! `scfsi`, 9-bit `scalefac_compress`, and no `preflag`.
//!
//! Region counts for window-switched (non-long) blocks are *not* transmitted —
//! they're derived later by the Huffman stage — so this parser only reads the
//! bits that are actually present.

use rff_core::{Error, Result};

use crate::bitio::BitReader;
use crate::frame::{BlockType, SideInfo};
use crate::header::{FrameHeader, MpegVersion};

/// Parse the side-information block into a [`SideInfo`].
pub fn parse(header: &FrameHeader, bytes: &[u8]) -> Result<SideInfo> {
    let mut r = BitReader::new(bytes);
    let mut si = SideInfo::default();
    let nch = header.channel_mode.channels();
    let mpeg1 = matches!(header.version, MpegVersion::V1);

    if mpeg1 {
        si.main_data_begin = r.read(9) as u16;
        r.read(if nch == 1 { 5 } else { 3 }); // private bits
        for ch in 0..nch {
            for band in 0..4 {
                si.scfsi[ch][band] = r.read_bool();
            }
        }
    } else {
        si.main_data_begin = r.read(8) as u16;
        r.read(if nch == 1 { 1 } else { 2 }); // private bits
    }

    for gr in 0..header.version.granules() {
        for ch in 0..nch {
            let g = &mut si.granules[gr][ch];
            g.part2_3_length = r.read(12) as u16;
            g.big_values = r.read(9) as u16;
            g.global_gain = r.read(8) as u8;
            g.scalefac_compress = r.read(if mpeg1 { 4 } else { 9 }) as u16;
            g.window_switching = r.read_bool();
            if g.window_switching {
                g.block_type = match r.read(2) {
                    1 => BlockType::Start,
                    2 => BlockType::Short,
                    3 => BlockType::Stop,
                    // block_type 0 (Long) is invalid when window switching is set.
                    _ => {
                        return Err(Error::invalid(
                            "mp3 side-info: long block_type with window switching",
                        ))
                    }
                };
                g.mixed_block = r.read_bool();
                for t in g.table_select.iter_mut().take(2) {
                    *t = r.read(5) as u8;
                }
                for sg in g.subblock_gain.iter_mut() {
                    *sg = r.read(3) as u8;
                }
                // region0/region1_count are implied for switched blocks.
            } else {
                g.block_type = BlockType::Long;
                for t in g.table_select.iter_mut() {
                    *t = r.read(5) as u8;
                }
                g.region0_count = r.read(4) as u8;
                g.region1_count = r.read(3) as u8;
            }
            if mpeg1 {
                g.preflag = r.read_bool();
            } else {
                // LSF carries no preflag bit — it's DERIVED: the pretab applies when
                // the scalefactor scheme's blocknumber is 2 (scalefac_compress ≥ 500).
                g.preflag = g.scalefac_compress >= 500;
            }
            g.scalefac_scale = r.read_bool();
            g.count1table_select = r.read_bool();
        }
    }

    // The side-information block is exactly `side_info_len` bytes and fully
    // consumed; a mismatch means a field-width bug in this parser.
    debug_assert_eq!(
        r.bit_pos(),
        header.side_info_len() * 8,
        "mp3 side-info bit accounting"
    );
    Ok(si)
}
