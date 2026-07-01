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
#[derive(Debug, Clone, Default, PartialEq, Eq)]
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
            // Short scalefactors are stored band-major (each band's three windows
            // together): for sfb, for window. slen1 covers bands < 6, slen2 the
            // rest. Reading them window-major scrambles values when slen1 ≠ slen2.
            let start = if gi.mixed_block {
                // Mixed: long bands 0..8 (slen1), then short bands 3..12.
                for b in 0..8 {
                    sf.long[b] = r.read(s1) as u8;
                }
                3
            } else {
                0
            };
            for sfb in start..12 {
                let slen = if sfb < 6 { s1 } else { s2 };
                for window in 0..3 {
                    sf.short[window][sfb] = r.read(slen) as u8;
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
    } else {
        // MPEG-2/2.5 (LSF): bit-lengths + per-group band counts are DERIVED from
        // scalefac_compress (no fixed table), and there's one granule/frame so no
        // scfsi reuse. Non-intensity channels only (the i_stereo right channel uses
        // a different derivation — not emitted here, and rare).
        //
        // BRING-UP STATUS: this scalefactor scheme is implemented and lifts LSF decode
        // from broken (−5.9 dB vs FFmpeg) to ~15 dB, but LSF is NOT yet bit-exact — a
        // separate STRUCTURAL bug remains (a pure sine @160k, negligible scalefactors,
        // still reads 17.6 dB), so the residual is independent of this code. Full LSF
        // conformance needs a dedicated bringup-decoder pass (symbol-by-symbol vs FFmpeg).
        let is_short = gi.window_switching && gi.block_type == BlockType::Short;
        let blocktype = if is_short {
            if gi.mixed_block {
                2
            } else {
                1
            }
        } else {
            0
        };
        let (slen, nr) = tables::lsf_scale_params(gi.scalefac_compress, blocktype);
        if is_short {
            // Groups fill (sfb, window) linearly: idx = sfb·3 + window.
            let mut idx = 0usize;
            for g in 0..4 {
                for _ in 0..nr[g] {
                    let v = r.read(slen[g] as u32) as u8;
                    let (sfb, window) = (idx / 3, idx % 3);
                    if sfb < crate::frame::SFB_SHORT {
                        sf.short[window][sfb] = v;
                    }
                    idx += 1;
                }
            }
        } else {
            // Long: groups fill scalefactor bands linearly.
            let mut sfb = 0usize;
            for g in 0..4 {
                for _ in 0..nr[g] {
                    let v = r.read(slen[g] as u32) as u8;
                    if sfb < crate::frame::SFB_LONG {
                        sf.long[sfb] = v;
                    }
                    sfb += 1;
                }
            }
        }
    }

    *bit_pos = r.bit_pos();
    sf
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::bitio::BitWriter;
    use crate::frame::{BlockType, ChannelMode, GranuleSideInfo};

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
    fn short_block_scalefactors_are_band_major() {
        // Short scalefactors are stored band-major; with slen1 ≠ slen2 a
        // window-major read scrambles them (the bug that broke window-switching).
        // scalefac_compress 12 → (slen1, slen2) = (3, 2).
        let mut w = BitWriter::new();
        let mut expect = [[0u8; crate::frame::SFB_SHORT]; 3];
        for sfb in 0..12 {
            let slen = if sfb < 6 { 3 } else { 2 };
            for window in 0..3 {
                let v = ((sfb * 3 + window) as u32) & ((1 << slen) - 1);
                w.write(v, slen);
                expect[window][sfb] = v as u8;
            }
        }
        let data = w.finish();

        let mut si = SideInfo::default();
        si.granules[0][0] = GranuleSideInfo {
            scalefac_compress: 12,
            window_switching: true,
            block_type: BlockType::Short,
            ..Default::default()
        };
        let mut pos = 0;
        let sf = decode(&data, &mut pos, &hdr(), &si, 0, 0, None);
        assert_eq!(sf.short, expect);
        assert_eq!(pos, 3 * (6 * 3 + 6 * 2), "band-major bit accounting");
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
        si.granules[0][0] = GranuleSideInfo {
            scalefac_compress: 15,
            ..Default::default()
        };
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
        si.granules[1][0] = GranuleSideInfo {
            scalefac_compress: 0,
            ..Default::default()
        };

        // scalefac_compress 0 → (0,0): no bits read for the non-reused bands.
        let mut pos = 0;
        let sf = decode(&[0u8; 4], &mut pos, &hdr(), &si, 1, 0, Some(&prev));
        assert_eq!(&sf.long[0..6], &[1, 2, 3, 4, 5, 6]);
        assert_eq!(pos, 0, "reused + zero-length bands read no bits");
    }
}
