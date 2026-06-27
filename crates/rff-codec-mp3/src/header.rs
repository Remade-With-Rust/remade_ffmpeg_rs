//! MP3 frame header — the 32-bit sync header that prefixes every frame.
//!
//! ```text
//!  syncword(11) | version(2) | layer(2) | crc(1)
//!  bitrate_idx(4) | samplerate_idx(2) | padding(1) | private(1)
//!  channel_mode(2) | mode_ext(2) | copyright(1) | original(1) | emphasis(2)
//! ```
//!
//! From these fields we derive the frame size in bytes and the per-frame sample
//! count, which the demux/packetizer needs to walk the stream.

use rff_core::{Error, Result};

use crate::frame::ChannelMode;
use crate::tables;

/// MPEG audio version (the sample-rate base differs per version).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MpegVersion {
    /// MPEG-1 — 2 granules/frame, 1152 samples/frame.
    V1,
    /// MPEG-2 (LSF) — 1 granule/frame, 576 samples/frame.
    V2,
    /// MPEG-2.5 (unofficial low-sample-rate extension).
    V2_5,
}

impl MpegVersion {
    /// Granules per frame: 2 for MPEG-1, 1 for MPEG-2/2.5.
    pub fn granules(self) -> usize {
        match self {
            MpegVersion::V1 => 2,
            _ => 1,
        }
    }
    /// Decoded PCM samples per channel per frame.
    pub fn samples_per_frame(self) -> usize {
        self.granules() * crate::frame::GRANULE_LINES
    }
}

/// A parsed Layer III frame header.
#[derive(Debug, Clone)]
pub struct FrameHeader {
    pub version: MpegVersion,
    pub crc_protected: bool,
    pub bitrate_kbps: u32,
    pub sample_rate: u32,
    pub padding: bool,
    pub channel_mode: ChannelMode,
    pub copyright: bool,
    pub original: bool,
    pub emphasis: u8,
}

impl FrameHeader {
    /// Parse a 4-byte header. Returns `Error::invalid` on a bad sync word or a
    /// reserved/free field this decoder doesn't accept yet.
    pub fn parse(bytes: [u8; 4]) -> Result<FrameHeader> {
        let h = u32::from_be_bytes(bytes);

        // 11-bit frame sync: all ones.
        if (h >> 21) & 0x7FF != 0x7FF {
            return Err(Error::invalid("mp3 header: bad frame sync"));
        }
        let version = match (h >> 19) & 0x3 {
            0b00 => MpegVersion::V2_5,
            0b10 => MpegVersion::V2,
            0b11 => MpegVersion::V1,
            _ => return Err(Error::invalid("mp3 header: reserved MPEG version")),
        };
        // Layer field: 0b01 == Layer III. This codec only does Layer III.
        if (h >> 17) & 0x3 != 0b01 {
            return Err(Error::unsupported("mp3 header: only Layer III is supported"));
        }
        let crc_protected = (h >> 16) & 1 == 0;

        let bitrate_index = ((h >> 12) & 0xF) as usize;
        if bitrate_index == 0 {
            return Err(Error::unsupported("mp3 header: free-format bitrate"));
        }
        if bitrate_index == 15 {
            return Err(Error::invalid("mp3 header: reserved bitrate index"));
        }
        let bitrate_kbps = match version {
            MpegVersion::V1 => tables::BITRATE_V1_L3[bitrate_index],
            _ => tables::BITRATE_V2_L3[bitrate_index],
        };

        let samplerate_index = ((h >> 10) & 0x3) as usize;
        if samplerate_index == 3 {
            return Err(Error::invalid("mp3 header: reserved sample-rate index"));
        }
        // Base rates are MPEG-1; MPEG-2 halves them and MPEG-2.5 quarters them.
        let base = tables::SAMPLE_RATE[samplerate_index];
        let sample_rate = match version {
            MpegVersion::V1 => base,
            MpegVersion::V2 => base / 2,
            MpegVersion::V2_5 => base / 4,
        };

        let padding = (h >> 9) & 1 == 1;
        let mode_ext = ((h >> 4) & 0x3) as u8;
        let channel_mode = match (h >> 6) & 0x3 {
            0b00 => ChannelMode::Stereo,
            // mode_extension bit 1 (value 2) = MS, bit 0 (value 1) = intensity.
            0b01 => ChannelMode::JointStereo {
                ms_stereo: mode_ext & 0x2 != 0,
                intensity_stereo: mode_ext & 0x1 != 0,
            },
            0b10 => ChannelMode::DualMono,
            _ => ChannelMode::Mono,
        };

        Ok(FrameHeader {
            version,
            crc_protected,
            bitrate_kbps,
            sample_rate,
            padding,
            channel_mode,
            copyright: (h >> 3) & 1 == 1,
            original: (h >> 2) & 1 == 1,
            emphasis: (h & 0x3) as u8,
        })
    }

    /// Serialize back to 4 bytes (encoder side) — the exact inverse of [`parse`].
    pub fn to_bytes(&self) -> [u8; 4] {
        let mut h: u32 = 0x7FF << 21; // frame sync
        let version = match self.version {
            MpegVersion::V2_5 => 0b00,
            MpegVersion::V2 => 0b10,
            MpegVersion::V1 => 0b11,
        };
        h |= version << 19;
        h |= 0b01 << 17; // Layer III
        h |= (!self.crc_protected as u32) << 16;

        let br_table: &[u32; 16] = match self.version {
            MpegVersion::V1 => &tables::BITRATE_V1_L3,
            _ => &tables::BITRATE_V2_L3,
        };
        let br_idx = br_table.iter().position(|&b| b == self.bitrate_kbps).unwrap_or(0) as u32;
        h |= br_idx << 12;

        let base = match self.version {
            MpegVersion::V1 => self.sample_rate,
            MpegVersion::V2 => self.sample_rate * 2,
            MpegVersion::V2_5 => self.sample_rate * 4,
        };
        let sr_idx = tables::SAMPLE_RATE.iter().position(|&s| s == base).unwrap_or(0) as u32;
        h |= sr_idx << 10;
        h |= (self.padding as u32) << 9;

        let (chan, ext) = match self.channel_mode {
            ChannelMode::Stereo => (0b00, 0),
            ChannelMode::JointStereo { ms_stereo, intensity_stereo } => {
                (0b01, (ms_stereo as u32) << 1 | intensity_stereo as u32)
            }
            ChannelMode::DualMono => (0b10, 0),
            ChannelMode::Mono => (0b11, 0),
        };
        h |= chan << 6;
        h |= ext << 4;
        h |= (self.copyright as u32) << 3;
        h |= (self.original as u32) << 2;
        h |= self.emphasis as u32;
        h.to_be_bytes()
    }

    /// Total frame size in bytes, including the header and optional CRC.
    ///
    /// `floor(samples_per_frame / 8 * bitrate / sample_rate) + padding`.
    pub fn frame_size(&self) -> usize {
        let spf = self.version.samples_per_frame();
        let bytes = (spf / 8) * (self.bitrate_kbps as usize * 1000) / self.sample_rate as usize;
        bytes + self.padding as usize
    }

    /// Bytes of side information following the header (+CRC): depends on version
    /// and channel count (MPEG-1 stereo: 32, mono: 17; MPEG-2 stereo: 17, mono: 9).
    pub fn side_info_len(&self) -> usize {
        let stereo = self.channel_mode.channels() == 2;
        match (self.version, stereo) {
            (MpegVersion::V1, true) => 32,
            (MpegVersion::V1, false) => 17,
            (_, true) => 17,
            (_, false) => 9,
        }
    }
}
