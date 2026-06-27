//! Scalefactor decoding.
//!
//! Each scalefactor band gets a small integer that scales its requantization
//! step. Bit-lengths come from `scalefac_compress` → (slen1, slen2): slen1 for
//! the low bands, slen2 for the high bands. For MPEG-1 long blocks the four
//! `scfsi` flags let granule 1 reuse granule 0's scalefactors per band group.
//! Short blocks store three sets (one per window) and never share.

use crate::bitio::BitReader;
use crate::frame::{BlockType, SideInfo};
use crate::header::{FrameHeader, MpegVersion};
use crate::tables;

/// Decoded scalefactors for one granule/channel.
#[derive(Debug, Clone, Default)]
pub struct ScaleFactors {
    /// Long-block bands `[0..21)` (band 21 is not coded).
    pub long: [u8; crate::frame::SFB_LONG],
    /// Short-block bands `[window][0..12)`.
    pub short: [[u8; crate::frame::SFB_SHORT]; 3],
}

/// scfsi band groups (long blocks): which long bands each of the 4 flags covers.
const SCFSI_GROUPS: [(usize, usize); 4] = [(0, 6), (6, 11), (11, 16), (16, 21)];

/// Read one granule/channel's scalefactors from the main-data bitstream,
/// advancing `*bit_pos`. `prev` is granule 0's scalefactors (for granule 1
/// `scfsi` reuse); pass `None` for granule 0.
pub fn decode(
    main: &[u8],
    bit_pos: &mut usize,
    header: &FrameHeader,
    si: &SideInfo,
    gr: usize,
    ch: usize,
    prev: Option<&ScaleFactors>,
) -> ScaleFactors {
    let mut r = BitReader::new(main);
    r.seek_bits(*bit_pos);
    let gi = &si.granules[gr][ch];
    let mut sf = ScaleFactors::default();

    if matches!(header.version, MpegVersion::V1) {
        let (slen1, slen2) = tables::SCALEFAC_COMPRESS_V1[gi.scalefac_compress as usize & 0xF];
        let (s1, s2) = (slen1 as u32, slen2 as u32);

        if gi.window_switching && gi.block_type == BlockType::Short {
            if gi.mixed_block {
                // Mixed: long bands 0..8 (slen1), then short bands 3..12.
                for b in 0..8 {
                    sf.long[b] = r.read(s1) as u8;
                }
                for w in 0..3 {
                    for b in 3..6 {
                        sf.short[w][b] = r.read(s1) as u8;
                    }
                    for b in 6..12 {
                        sf.short[w][b] = r.read(s2) as u8;
                    }
                }
            } else {
                for w in 0..3 {
                    for b in 0..6 {
                        sf.short[w][b] = r.read(s1) as u8;
                    }
                    for b in 6..12 {
                        sf.short[w][b] = r.read(s2) as u8;
                    }
                }
            }
        } else {
            // Long block: bands 0..21, with optional scfsi reuse in granule 1.
            for (g, &(lo, hi)) in SCFSI_GROUPS.iter().enumerate() {
                for b in lo..hi {
                    let slen = if b < 11 { s1 } else { s2 };
                    if gr == 1 && si.scfsi[ch][g] {
                        sf.long[b] = prev.map_or(0, |p| p.long[b]);
                    } else {
                        sf.long[b] = r.read(slen) as u8;
                    }
                }
            }
        }
    }
    // brick: MPEG-2/2.5 scalefactor scheme (intensity-stereo-aware, derived slen).

    *bit_pos = r.bit_pos();
    sf
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::bitio::BitWriter;
    use crate::frame::{ChannelMode, GranuleSideInfo};

    fn hdr() -> FrameHeader {
        FrameHeader {
            version: MpegVersion::V1,
            crc_protected: false,
            bitrate_kbps: 128,
            sample_rate: 44100,
            padding: false,
            channel_mode: ChannelMode::Mono,
            copyright: false,
            original: true,
            emphasis: 0,
        }
    }

    #[test]
    fn long_block_scalefactors_and_bit_accounting() {
        // scalefac_compress 15 → (slen1, slen2) = (4, 3): bands 0..11 use 4 bits,
        // bands 11..21 use 3 bits. 11*4 + 10*3 = 74 bits total.
        let mut w = BitWriter::new();
        let mut expect = [0u8; 22];
        for b in 0..21 {
            let slen = if b < 11 { 4 } else { 3 };
            let v = (b as u32 + 1) & ((1 << slen) - 1);
            w.write(v, slen);
            expect[b] = v as u8;
        }
        let data = w.finish();

        let mut si = SideInfo::default();
        si.granules[0][0] = GranuleSideInfo { scalefac_compress: 15, ..Default::default() };
        let mut pos = 0;
        let sf = decode(&data, &mut pos, &hdr(), &si, 0, 0, None);

        assert_eq!(sf.long[..21], expect[..21]);
        assert_eq!(pos, 74, "must consume exactly the scalefactor bits");
    }

    #[test]
    fn scfsi_reuses_granule0_in_granule1() {
        // Granule 1 with scfsi group 0 set reuses granule 0's bands 0..6.
        let mut prev = ScaleFactors::default();
        for b in 0..6 {
            prev.long[b] = (b as u8) + 1;
        }
        let mut si = SideInfo::default();
        si.scfsi[0][0] = true; // reuse group 0 (bands 0..6)
        si.granules[1][0] = GranuleSideInfo { scalefac_compress: 0, ..Default::default() };

        // scalefac_compress 0 → (0,0): no bits read for the non-reused bands.
        let mut pos = 0;
        let sf = decode(&[0u8; 4], &mut pos, &hdr(), &si, 1, 0, Some(&prev));
        assert_eq!(&sf.long[0..6], &[1, 2, 3, 4, 5, 6]);
        assert_eq!(pos, 0, "reused + zero-length bands read no bits");
    }
}
