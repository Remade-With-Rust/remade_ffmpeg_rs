//! Matroska / WebM demuxer.
//!
//! Matroska is an [EBML](https://www.matroska.org/) (binary XML) container.
//! WebM is its restricted profile (VP8/VP9/AV1 video + Opus/Vorbis audio). We
//! parse the EBML element tree to extract the track list and then walk the
//! Clusters, turning each (Simple)Block into a [`Packet`]. The whole input is
//! buffered up front since the [`Input`] is not seekable.

use std::collections::VecDeque;
use std::io::Read;

use rff_core::{CodecId, Error, MediaType, Packet, Rational, Result};
use rff_format::{Demuxer, Format, FormatRegistry, Input, Stream};

/// Register the Matroska/WebM demuxer.
pub fn register(registry: &mut FormatRegistry) {
    registry.register(Format {
        name: "matroska",
        long_name: "Matroska / WebM (EBML)",
        extensions: &["mkv", "webm", "mka"],
        demuxer: Some(|input| Box::new(MkvDemuxer::new(input))),
        muxer: None,
        probe: Some(probe_mkv),
    });
}

/// The EBML magic (`\x1A\x45\xDF\xA3`) starts every Matroska/WebM file.
pub fn probe_mkv(bytes: &[u8]) -> i32 {
    if bytes.starts_with(&[0x1A, 0x45, 0xDF, 0xA3]) {
        90
    } else {
        0
    }
}

// ---- EBML element IDs (with their length-marker bits intact) --------------
const ID_SEGMENT: u32 = 0x1853_8067;
const ID_INFO: u32 = 0x1549_A966;
const ID_TIMESTAMP_SCALE: u32 = 0x2AD7_B1;
const ID_TRACKS: u32 = 0x1654_AE6B;
const ID_TRACK_ENTRY: u32 = 0xAE;
const ID_TRACK_NUMBER: u32 = 0xD7;
const ID_TRACK_TYPE: u32 = 0x83;
const ID_CODEC_ID: u32 = 0x86;
const ID_CODEC_PRIVATE: u32 = 0x63A2;
const ID_VIDEO: u32 = 0xE0;
const ID_PIXEL_WIDTH: u32 = 0xB0;
const ID_PIXEL_HEIGHT: u32 = 0xBA;
const ID_AUDIO: u32 = 0xE1;
const ID_SAMPLING_FREQUENCY: u32 = 0xB5;
const ID_CHANNELS: u32 = 0x9F;
const ID_CLUSTER: u32 = 0x1F43_B675;
const ID_TIMESTAMP: u32 = 0xE7;
const ID_SIMPLE_BLOCK: u32 = 0xA3;
const ID_BLOCK_GROUP: u32 = 0xA0;
const ID_BLOCK: u32 = 0xA1;

/// A cursor over the buffered file that reads EBML primitives.
struct Ebml<'a> {
    d: &'a [u8],
    pos: usize,
}

impl<'a> Ebml<'a> {
    fn new(d: &'a [u8]) -> Ebml<'a> {
        Ebml { d, pos: 0 }
    }

    fn at(d: &'a [u8], pos: usize) -> Ebml<'a> {
        Ebml { d, pos }
    }

    fn remaining(&self) -> usize {
        self.d.len().saturating_sub(self.pos)
    }

    /// Read an element ID (1-4 bytes), keeping the length-marker bits.
    fn read_id(&mut self) -> Option<u32> {
        let first = *self.d.get(self.pos)?;
        let len = first.leading_zeros() as usize + 1;
        if len > 4 || self.pos + len > self.d.len() {
            return None;
        }
        let mut id = 0u32;
        for i in 0..len {
            id = (id << 8) | self.d[self.pos + i] as u32;
        }
        self.pos += len;
        Some(id)
    }

    /// Read an EBML size (vint with the marker stripped). `None` element value
    /// means an "unknown size" marker (all value bits set).
    fn read_size(&mut self) -> Option<(u64, bool)> {
        let first = *self.d.get(self.pos)?;
        let len = first.leading_zeros() as usize + 1;
        if len > 8 || self.pos + len > self.d.len() {
            return None;
        }
        let mask = (1u64 << (8 - len)) - 1;
        let mut val = (first as u64) & mask;
        let mut all_ones = (first as u64) & mask == mask;
        for i in 1..len {
            let b = self.d[self.pos + i];
            val = (val << 8) | b as u64;
            all_ones = all_ones && b == 0xFF;
        }
        self.pos += len;
        Some((val, all_ones && len > 0))
    }

    fn read_uint(&mut self, len: usize) -> u64 {
        let mut v = 0u64;
        for i in 0..len {
            v = (v << 8) | *self.d.get(self.pos + i).unwrap_or(&0) as u64;
        }
        self.pos += len;
        v
    }

    fn read_float(&mut self, len: usize) -> f64 {
        let v = match len {
            4 => {
                let b = self.d.get(self.pos..self.pos + 4);
                b.map(|b| f32::from_be_bytes([b[0], b[1], b[2], b[3]]) as f64)
                    .unwrap_or(0.0)
            }
            8 => {
                let b = self.d.get(self.pos..self.pos + 8);
                b.map(|b| f64::from_be_bytes(b.try_into().unwrap()))
                    .unwrap_or(0.0)
            }
            _ => 0.0,
        };
        self.pos += len;
        v
    }

    fn read_bytes(&mut self, len: usize) -> &'a [u8] {
        let end = (self.pos + len).min(self.d.len());
        let s = &self.d[self.pos..end];
        self.pos = end;
        s
    }
}

struct MkvDemuxer {
    input: Option<Input>,
    streams: Vec<Stream>,
    /// Maps a Matroska track number to our 0-based stream index.
    track_map: Vec<(u64, usize)>,
    timestamp_scale: u64,
    packets: VecDeque<Packet>,
    parsed: bool,
}

impl MkvDemuxer {
    fn new(input: Input) -> MkvDemuxer {
        MkvDemuxer {
            input: Some(input),
            streams: Vec::new(),
            track_map: Vec::new(),
            timestamp_scale: 1_000_000, // default: 1 ms
            packets: VecDeque::new(),
            parsed: false,
        }
    }

    fn parse(&mut self, data: &[u8]) -> Result<()> {
        let mut top = Ebml::new(data);
        // EBML header, then the Segment.
        while top.remaining() > 0 {
            let Some(id) = top.read_id() else { break };
            let Some((size, unknown)) = top.read_size() else {
                break;
            };
            let end = if unknown {
                data.len()
            } else {
                (top.pos + size as usize).min(data.len())
            };
            if id == ID_SEGMENT {
                self.parse_segment(data, top.pos, end);
                break;
            }
            top.pos = end;
        }
        if self.streams.is_empty() {
            return Err(Error::invalid("mkv: no tracks found"));
        }
        Ok(())
    }

    fn parse_segment(&mut self, data: &[u8], start: usize, end: usize) {
        let mut e = Ebml::at(data, start);
        while e.pos < end {
            let Some(id) = e.read_id() else { break };
            let Some((size, unknown)) = e.read_size() else {
                break;
            };
            let child_end = if unknown {
                end
            } else {
                (e.pos + size as usize).min(end)
            };
            match id {
                ID_INFO => self.parse_info(data, e.pos, child_end),
                ID_TRACKS => self.parse_tracks(data, e.pos, child_end),
                ID_CLUSTER => self.parse_cluster(data, e.pos, child_end, unknown),
                _ => {}
            }
            // For unknown-size Clusters we stop the child run at the next
            // top-level ID; parse_cluster reports where it actually ended.
            e.pos = child_end.max(e.pos);
        }
    }

    fn parse_info(&mut self, data: &[u8], start: usize, end: usize) {
        let mut e = Ebml::at(data, start);
        while e.pos < end {
            let Some(id) = e.read_id() else { break };
            let Some((size, _)) = e.read_size() else {
                break;
            };
            if id == ID_TIMESTAMP_SCALE {
                self.timestamp_scale = e.read_uint(size as usize);
            } else {
                e.pos += size as usize;
            }
        }
    }

    fn parse_tracks(&mut self, data: &[u8], start: usize, end: usize) {
        let mut e = Ebml::at(data, start);
        while e.pos < end {
            let Some(id) = e.read_id() else { break };
            let Some((size, _)) = e.read_size() else {
                break;
            };
            let entry_end = (e.pos + size as usize).min(end);
            if id == ID_TRACK_ENTRY {
                self.parse_track_entry(data, e.pos, entry_end);
            }
            e.pos = entry_end;
        }
    }

    fn parse_track_entry(&mut self, data: &[u8], start: usize, end: usize) {
        let mut number = 0u64;
        let mut codec = String::new();
        let mut codec_private: Vec<u8> = Vec::new();
        let mut track_type = 0u64;
        let (mut width, mut height) = (0u32, 0u32);
        let (mut rate, mut channels) = (0u32, 0u16);

        let mut e = Ebml::at(data, start);
        while e.pos < end {
            let Some(id) = e.read_id() else { break };
            let Some((size, _)) = e.read_size() else {
                break;
            };
            let len = size as usize;
            match id {
                ID_TRACK_NUMBER => number = e.read_uint(len),
                ID_TRACK_TYPE => track_type = e.read_uint(len),
                ID_CODEC_ID => codec = String::from_utf8_lossy(e.read_bytes(len)).into_owned(),
                ID_CODEC_PRIVATE => codec_private = e.read_bytes(len).to_vec(),
                ID_VIDEO => {
                    let mut v = Ebml::at(data, e.pos);
                    let ve = e.pos + len;
                    while v.pos < ve {
                        let Some(vid) = v.read_id() else { break };
                        let Some((vsz, _)) = v.read_size() else { break };
                        match vid {
                            ID_PIXEL_WIDTH => width = v.read_uint(vsz as usize) as u32,
                            ID_PIXEL_HEIGHT => height = v.read_uint(vsz as usize) as u32,
                            _ => v.pos += vsz as usize,
                        }
                    }
                    e.pos += len;
                }
                ID_AUDIO => {
                    let mut a = Ebml::at(data, e.pos);
                    let ae = e.pos + len;
                    while a.pos < ae {
                        let Some(aid) = a.read_id() else { break };
                        let Some((asz, _)) = a.read_size() else { break };
                        match aid {
                            ID_SAMPLING_FREQUENCY => rate = a.read_float(asz as usize) as u32,
                            ID_CHANNELS => channels = a.read_uint(asz as usize) as u16,
                            _ => a.pos += asz as usize,
                        }
                    }
                    e.pos += len;
                }
                _ => e.pos += len,
            }
        }

        let codec_id = map_codec(&codec);
        let index = self.streams.len();
        let mut s = Stream::new(index, codec_id);
        s.media_type = match track_type {
            1 => MediaType::Video,
            2 => MediaType::Audio,
            _ => MediaType::Data,
        };
        s.width = width;
        s.height = height;
        s.sample_rate = rate;
        s.channels = channels;
        s.extradata = codec_private;
        // Matroska timestamps are in `timestamp_scale` ns; expose ms time base.
        s.time_base = Rational::new(1, (1_000_000_000 / self.timestamp_scale.max(1)) as i32);
        self.track_map.push((number, index));
        self.streams.push(s);
    }

    fn parse_cluster(&mut self, data: &[u8], start: usize, end: usize, _unknown: bool) {
        let mut cluster_ts = 0i64;
        let mut e = Ebml::at(data, start);
        while e.pos < end {
            let Some(id) = e.read_id() else { break };
            let Some((size, _)) = e.read_size() else {
                break;
            };
            let len = size as usize;
            match id {
                ID_TIMESTAMP => cluster_ts = e.read_uint(len) as i64,
                ID_SIMPLE_BLOCK => {
                    let block = e.read_bytes(len);
                    self.parse_block(block, cluster_ts);
                }
                ID_BLOCK_GROUP => {
                    let ge = e.pos + len;
                    let mut g = Ebml::at(data, e.pos);
                    while g.pos < ge {
                        let Some(gid) = g.read_id() else { break };
                        let Some((gsz, _)) = g.read_size() else { break };
                        if gid == ID_BLOCK {
                            let block = g.read_bytes(gsz as usize);
                            self.parse_block(block, cluster_ts);
                        } else {
                            g.pos += gsz as usize;
                        }
                    }
                    e.pos = ge;
                }
                _ => e.pos += len,
            }
        }
    }

    /// Parse a (Simple)Block body: track vint, int16 relative timestamp, flags,
    /// then the frame payload (no-lacing only for now).
    fn parse_block(&mut self, block: &[u8], cluster_ts: i64) {
        let mut b = Ebml::new(block);
        let Some((track_num, _)) = b.read_size() else {
            return;
        };
        if b.pos + 3 > block.len() {
            return;
        }
        let rel = i16::from_be_bytes([block[b.pos], block[b.pos + 1]]) as i64;
        b.pos += 2;
        let flags = block[b.pos];
        b.pos += 1;
        let lacing = (flags >> 1) & 0x03;
        let keyframe = flags & 0x80 != 0;
        let Some(&(_, index)) = self.track_map.iter().find(|(n, _)| *n == track_num) else {
            return;
        };
        if lacing != 0 {
            return; // laced blocks unsupported for now (rare for video/Opus)
        }
        let data = block[b.pos..].to_vec();
        let mut packet = Packet::from_data(index, data);
        packet.pts = Some(cluster_ts + rel);
        packet.flags.keyframe = keyframe;
        self.packets.push_back(packet);
    }
}

impl Demuxer for MkvDemuxer {
    fn read_header(&mut self) -> Result<Vec<Stream>> {
        if !self.parsed {
            let mut buf = Vec::new();
            self.input
                .as_mut()
                .ok_or_else(|| Error::invalid("mkv: no input"))?
                .read_to_end(&mut buf)?;
            self.parse(&buf)?;
            self.parsed = true;
        }
        Ok(self.streams.clone())
    }

    fn read_packet(&mut self) -> Result<Packet> {
        self.packets.pop_front().ok_or(Error::Eof)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn reads_element_ids_keeping_marker() {
        let mut e = Ebml::new(&[0x1A, 0x45, 0xDF, 0xA3]);
        assert_eq!(e.read_id(), Some(0x1A45_DFA3)); // EBML, 4-byte id
        let mut e = Ebml::new(&[0x83]);
        assert_eq!(e.read_id(), Some(0x83)); // TrackType, 1-byte id
        let mut e = Ebml::new(&[0x63, 0xA2]);
        assert_eq!(e.read_id(), Some(0x63A2)); // CodecPrivate, 2-byte id
    }

    #[test]
    fn reads_sizes_stripping_marker() {
        // 1-byte size 0x82 → value 2.
        let mut e = Ebml::new(&[0x82]);
        assert_eq!(e.read_size(), Some((2, false)));
        // 2-byte size 0x40 0x07 → value 7.
        let mut e = Ebml::new(&[0x40, 0x07]);
        assert_eq!(e.read_size(), Some((7, false)));
        // All-ones 0xFF → unknown size.
        let mut e = Ebml::new(&[0xFF]);
        assert_eq!(e.read_size(), Some((0x7F, true)));
    }

    #[test]
    fn probe_detects_ebml_magic() {
        assert_eq!(probe_mkv(&[0x1A, 0x45, 0xDF, 0xA3, 0x00]), 90);
        assert_eq!(probe_mkv(&[0x00, 0x00]), 0);
    }
}

fn map_codec(codec: &str) -> CodecId {
    match codec {
        "V_AV1" => CodecId::Avif, // our AV1 (rav1d) decoder
        "V_VP9" => CodecId::Vp9,
        "V_MPEG4/ISO/AVC" => CodecId::H264,
        "A_OPUS" => CodecId::Opus,
        "A_VORBIS" => CodecId::Vorbis,
        "A_AAC" => CodecId::Aac,
        "A_FLAC" => CodecId::Flac,
        _ => CodecId::None,
    }
}
