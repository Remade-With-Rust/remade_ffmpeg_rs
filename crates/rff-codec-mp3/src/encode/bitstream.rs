//! Frame assembly + the encoder-side bit reservoir, and the side-info /
//! scalefactor serializers (bricks **B5**, **B6**, **B7**, **B8**).
//!
//! Writes the header (+ optional CRC), the side-information block, and the main
//! data. The reservoir lets a granule borrow unused bits donated by earlier
//! frames: `main_data_begin` records how far back this frame's main data starts,
//! so a complex granule can spend more than its nominal budget.

use crate::bitio::BitWriter;
use crate::decode::scalefactors::ScaleFactors;
use crate::frame::{BlockType, SideInfo};
use crate::header::{FrameHeader, MpegVersion};
use crate::tables;

/// scfsi band groups (long blocks) — the encode twin of decode's `SCFSI_GROUPS`.
const SCFSI_GROUPS: [(usize, usize); 4] = [(0, 6), (6, 11), (11, 16), (16, 21)];

// ── B5: side-information serializer ───────────────────────────────────────────

/// **B5** — serialize the side-information block, the exact inverse of
/// `decode/sideinfo.rs`. Produces exactly `header.side_info_len()` bytes (every
/// bit of the block is a defined field, so there is no padding). MPEG-1 only.
pub fn serialize_side_info(header: &FrameHeader, si: &SideInfo) -> Vec<u8> {
    let mut w = BitWriter::new();
    let nch = header.channel_mode.channels();
    let mpeg1 = matches!(header.version, MpegVersion::V1);

    if mpeg1 {
        w.write(si.main_data_begin as u32, 9);
        w.write(0, if nch == 1 { 5 } else { 3 }); // private bits
        for ch in 0..nch {
            for band in 0..4 {
                w.write(si.scfsi[ch][band] as u32, 1);
            }
        }
    } else {
        w.write(si.main_data_begin as u32, 8);
        w.write(0, if nch == 1 { 1 } else { 2 });
    }

    for gr in 0..header.version.granules() {
        for ch in 0..nch {
            let g = &si.granules[gr][ch];
            w.write(g.part2_3_length as u32, 12);
            w.write(g.big_values as u32, 9);
            w.write(g.global_gain as u32, 8);
            w.write(g.scalefac_compress as u32, if mpeg1 { 4 } else { 9 });
            w.write(g.window_switching as u32, 1);
            if g.window_switching {
                let bt = match g.block_type {
                    BlockType::Start => 1,
                    BlockType::Short => 2,
                    BlockType::Stop => 3,
                    // Long with window switching is invalid; the encoder never emits it.
                    BlockType::Long => 0,
                };
                w.write(bt, 2);
                w.write(g.mixed_block as u32, 1);
                for t in g.table_select.iter().take(2) {
                    w.write(*t as u32, 5);
                }
                for sg in &g.subblock_gain {
                    w.write(*sg as u32, 3);
                }
            } else {
                for t in &g.table_select {
                    w.write(*t as u32, 5);
                }
                w.write(g.region0_count as u32, 4);
                w.write(g.region1_count as u32, 3);
            }
            if mpeg1 {
                w.write(g.preflag as u32, 1);
            }
            w.write(g.scalefac_scale as u32, 1);
            w.write(g.count1table_select as u32, 1);
        }
    }
    w.finish()
}

// ── B6: scalefactor serializer ────────────────────────────────────────────────

/// **B6** — write one granule/channel's scalefactors into the main-data bitstream,
/// the inverse of `decode/scalefactors.rs`. Mirrors the band-major short-block
/// layout and the granule-1 `scfsi` reuse (skipped bands are not written). MPEG-1.
pub fn serialize_scalefactors(
    w: &mut BitWriter,
    header: &FrameHeader,
    si: &SideInfo,
    gr: usize,
    ch: usize,
    sf: &ScaleFactors,
) {
    if !matches!(header.version, MpegVersion::V1) {
        return; // brick: MPEG-2/2.5 scalefactor scheme
    }
    let gi = &si.granules[gr][ch];
    let (slen1, slen2) = tables::SCALEFAC_COMPRESS_V1[gi.scalefac_compress as usize & 0xF];
    let (s1, s2) = (slen1 as u32, slen2 as u32);

    if gi.window_switching && gi.block_type == BlockType::Short {
        // Band-major: for each sfb, its three windows (the decode-side gotcha).
        let start = if gi.mixed_block {
            for b in 0..8 {
                w.write(sf.long[b] as u32, s1);
            }
            3
        } else {
            0
        };
        for sfb in start..12 {
            let slen = if sfb < 6 { s1 } else { s2 };
            for window in 0..3 {
                w.write(sf.short[window][sfb] as u32, slen);
            }
        }
    } else {
        for (g, &(lo, hi)) in SCFSI_GROUPS.iter().enumerate() {
            for b in lo..hi {
                let slen = if b < 11 { s1 } else { s2 };
                // Granule 1 reuses granule 0's bands per scfsi — those aren't coded.
                if gr == 1 && si.scfsi[ch][g] {
                    continue;
                }
                w.write(sf.long[b] as u32, slen);
            }
        }
    }
}

// ── B7: frame assembly (B8 reservoir borrowing deferred) ──────────────────────

/// Encoder-side reservoir: how many spare main-data bytes are banked.
#[derive(Debug, Clone, Default)]
pub struct EncReservoir {
    /// Spare bytes carried forward for future frames to borrow.
    pub spare_bytes: usize,
}

/// **B7** — assemble one complete MP3 frame: header (+ optional CRC) + side info +
/// main data, padded to the frame size.
///
/// This runs **reservoir-free** (`main_data_begin = 0`): each frame's main data
/// must fit within its own region, and the unused tail is zero stuffing. That is a
/// fully valid, decodable CBR frame — the bit-reservoir *borrowing* (**B8**, using
/// `spare_bytes` to set `main_data_begin > 0`) is the quality optimisation layered
/// on next. The reservoir's banked slack is tracked here so B8 can consume it.
pub fn format(
    header: &FrameHeader,
    side_info: &SideInfo,
    main_data: &[u8],
    reservoir: &mut EncReservoir,
) -> Vec<u8> {
    let mut si = side_info.clone();
    si.main_data_begin = 0; // B8: reservoir borrowing not yet wired.

    let mut out = header.to_bytes().to_vec();
    if header.crc_protected {
        // brick (B7+): real CRC-16 over header bits 16..32 + the side-info block.
        // The decoder skips these two bytes without validating, so a placeholder
        // keeps the framing correct until the CRC is computed.
        out.extend_from_slice(&[0, 0]);
    }
    out.extend_from_slice(&serialize_side_info(header, &si));

    let region_cap = header.frame_size().saturating_sub(out.len());
    let used = main_data.len().min(region_cap);
    out.extend_from_slice(&main_data[..used]);
    out.resize(header.frame_size(), 0); // zero stuffing fills the rest

    // Bank the slack this frame leaves for B8 to borrow later.
    reservoir.spare_bytes = region_cap - used;
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::frame::{ChannelMode, GranuleSideInfo};
    use crate::header::MpegVersion;

    fn hdr(channel_mode: ChannelMode) -> FrameHeader {
        FrameHeader {
            version: MpegVersion::V1,
            crc_protected: false,
            bitrate_kbps: 128,
            sample_rate: 44100,
            padding: false,
            channel_mode,
            copyright: false,
            original: true,
            emphasis: 0,
        }
    }

    #[test]
    fn side_info_round_trips_stereo_long() {
        let header = hdr(ChannelMode::Stereo);
        let mut si = SideInfo {
            main_data_begin: 42,
            scfsi: [[true, false, true, false], [false, true, false, true]],
            ..Default::default()
        };
        for gr in 0..2 {
            for ch in 0..2 {
                si.granules[gr][ch] = GranuleSideInfo {
                    part2_3_length: (100 + gr * 10 + ch) as u16,
                    big_values: (200 + ch) as u16,
                    global_gain: (120 + gr * 4) as u8,
                    scalefac_compress: 9,
                    table_select: [3, 7, 11],
                    region0_count: 7,
                    region1_count: 2,
                    preflag: gr == 0,
                    scalefac_scale: ch == 1,
                    count1table_select: gr == 1,
                    ..Default::default()
                };
            }
        }

        let bytes = serialize_side_info(&header, &si);
        assert_eq!(bytes.len(), header.side_info_len());
        let parsed = crate::decode::sideinfo::parse(&header, &bytes).unwrap();
        assert_eq!(parsed, si);
    }

    #[test]
    fn side_info_round_trips_mono_short() {
        let header = hdr(ChannelMode::Mono);
        let mut si = SideInfo {
            main_data_begin: 7,
            ..Default::default()
        };
        si.granules[0][0] = GranuleSideInfo {
            part2_3_length: 333,
            big_values: 50,
            global_gain: 150,
            scalefac_compress: 5,
            window_switching: true,
            block_type: BlockType::Short,
            mixed_block: false,
            table_select: [5, 9, 0],
            subblock_gain: [1, 2, 3],
            ..Default::default()
        };
        si.granules[1][0] = GranuleSideInfo {
            part2_3_length: 120,
            big_values: 10,
            global_gain: 130,
            window_switching: true,
            block_type: BlockType::Start,
            table_select: [2, 4, 0],
            subblock_gain: [0, 0, 0],
            ..Default::default()
        };

        let bytes = serialize_side_info(&header, &si);
        assert_eq!(bytes.len(), header.side_info_len());
        let parsed = crate::decode::sideinfo::parse(&header, &bytes).unwrap();
        assert_eq!(parsed, si);
    }

    /// Serialize scalefactors then read them back with the decoder.
    fn sf_round_trip(
        si: &SideInfo,
        gr: usize,
        ch: usize,
        sf: &ScaleFactors,
        prev: Option<&ScaleFactors>,
    ) {
        let header = hdr(ChannelMode::Mono);
        let mut w = BitWriter::new();
        serialize_scalefactors(&mut w, &header, si, gr, ch, sf);
        let bits = w.finish();
        let mut pos = 0;
        let got = crate::decode::scalefactors::decode(&bits, &mut pos, &header, si, gr, ch, prev);
        assert_eq!(&got, sf);
    }

    #[test]
    fn scalefactors_round_trip_long() {
        let mut si = SideInfo::default();
        si.granules[0][0] = GranuleSideInfo {
            scalefac_compress: 15, // (slen1,slen2)=(4,3)
            ..Default::default()
        };
        let mut sf = ScaleFactors::default();
        for b in 0..21 {
            sf.long[b] = (b as u8) % (if b < 11 { 16 } else { 8 });
        }
        sf_round_trip(&si, 0, 0, &sf, None);
    }

    #[test]
    fn scalefactors_round_trip_short_band_major() {
        let mut si = SideInfo::default();
        si.granules[0][0] = GranuleSideInfo {
            scalefac_compress: 12, // (3,2)
            window_switching: true,
            block_type: BlockType::Short,
            ..Default::default()
        };
        let mut sf = ScaleFactors::default();
        for sfb in 0..12 {
            let cap = if sfb < 6 { 8 } else { 4 };
            for window in 0..3 {
                sf.short[window][sfb] = ((sfb * 3 + window) as u8) % cap;
            }
        }
        sf_round_trip(&si, 0, 0, &sf, None);
    }

    #[test]
    fn frame_assembly_is_decodable() {
        use crate::frame::GranuleSideInfo;
        use rff_codec::Decoder;
        use rff_core::{Frame, Packet};

        let header = hdr(ChannelMode::Mono);
        // A trivial-but-valid granule: no scalefactors, no big_values, no count1 —
        // decodes to silence. Both granules (MPEG-1) identical.
        let mut si = SideInfo::default();
        for gr in 0..2 {
            si.granules[gr][0] = GranuleSideInfo {
                part2_3_length: 0,
                ..Default::default()
            };
        }
        let main_data = [0u8; 10]; // a few bytes; the rest is stuffing

        let mut res = EncReservoir::default();
        let frame = format(&header, &si, &main_data, &mut res);

        // Structural checks.
        assert_eq!(frame.len(), header.frame_size());
        assert_eq!(&frame[0..4], &header.to_bytes());
        let si_bytes = &frame[4..4 + header.side_info_len()];
        assert_eq!(
            crate::decode::sideinfo::parse(&header, si_bytes).unwrap(),
            si
        );

        // The real decoder must accept two concatenated frames and yield two
        // audio frames of one frame's samples each.
        let mut stream = frame.clone();
        stream.extend_from_slice(&format(&header, &si, &main_data, &mut res));

        let mut dec = crate::Mp3Decoder::default();
        dec.send_packet(&Packet::from_data(0, stream)).unwrap();
        dec.flush();
        let mut frames = 0;
        while let Ok(Frame::Audio(af)) = dec.receive_frame() {
            assert_eq!(af.samples, header.version.samples_per_frame());
            frames += 1;
        }
        assert_eq!(frames, 2, "both assembled frames must decode");
    }

    #[test]
    fn scalefactors_round_trip_scfsi_reuse() {
        // Granule 1 reuses granule 0's group-0 bands (0..6): they aren't coded.
        let mut si = SideInfo::default();
        si.scfsi[0][0] = true;
        si.granules[1][0] = GranuleSideInfo {
            scalefac_compress: 15,
            ..Default::default()
        };
        let mut prev = ScaleFactors::default();
        for b in 0..6 {
            prev.long[b] = (b as u8) + 1;
        }
        // The reused bands must come back from `prev`; the rest from the stream.
        let mut sf = prev.clone();
        for b in 6..21 {
            sf.long[b] = (b as u8) % (if b < 11 { 16 } else { 8 });
        }
        sf_round_trip(&si, 1, 0, &sf, Some(&prev));
    }
}
